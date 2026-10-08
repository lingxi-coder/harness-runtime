//! Atomic job inheritance and suspended startup for supervised shell payloads.
//!
//! The supervisor joins its job before creating any payload. CreateProcess then
//! associates descendants atomically, including a child whose PID has not yet
//! been reported to the source. No assignment-after-spawn race is introduced.
#![allow(unsafe_code)]
use mobile_linux_api::ProcessError;
use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::OnceLock;

type Handle = *mut c_void;
#[repr(C)]
#[derive(Default)]
struct BasicLimits {
    process_time: i64,
    job_time: i64,
    flags: u32,
    minimum_working_set: usize,
    maximum_working_set: usize,
    active_process_limit: u32,
    affinity: usize,
    priority: u32,
    scheduling: u32,
}
#[repr(C)]
#[derive(Default)]
struct ExtendedLimits {
    basic: BasicLimits,
    io: [u64; 6],
    process_memory: usize,
    job_memory: usize,
    peak_process_memory: usize,
    peak_job_memory: usize,
}
#[repr(C)]
#[derive(Default)]
struct ThreadEntry {
    size: u32,
    usage: u32,
    id: u32,
    owner: u32,
    base_priority: i32,
    delta_priority: i32,
    flags: u32,
}
#[link(name = "kernel32")]
extern "system" {
    fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
    fn SetInformationJobObject(job: Handle, class: u32, info: *const c_void, len: u32) -> i32;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    fn TerminateJobObject(job: Handle, exit_code: u32) -> i32;
    fn GetCurrentProcess() -> Handle;
    fn GetHandleInformation(handle: Handle, flags: *mut u32) -> i32;
    fn IsProcessInJob(process: Handle, job: Handle, result: *mut i32) -> i32;
    fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> Handle;
    fn Thread32First(snapshot: Handle, entry: *mut ThreadEntry) -> i32;
    fn Thread32Next(snapshot: Handle, entry: *mut ThreadEntry) -> i32;
    fn OpenThread(access: u32, inherit: i32, id: u32) -> Handle;
    fn GetProcessIdOfThread(thread: Handle) -> u32;
    fn ResumeThread(thread: Handle) -> u32;
}

// Static handles are intentionally never dropped during normal Rust shutdown.
// The OS closes this unique non-inheritable job handle only when this dedicated
// supervisor exits. Closing a local guard would kill the supervisor itself.
static JOB: OnceLock<OwnedHandle> = OnceLock::new();
static INITIALIZE: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn error(context: &str) -> ProcessError {
    ProcessError::Io(format!("{context}: {}", std::io::Error::last_os_error()))
}

/// A payload-only job stays authoritative after the shell leader exits.
/// It is nested inside the supervisor job when supervision is enabled; killing
/// it leaves the supervisor alive to finish output and write its receipt.
pub(super) struct CommandJob(OwnedHandle);

impl CommandJob {
    pub(super) fn assign_suspended(child: &tokio::process::Child) -> Result<Self, ProcessError> {
        let process = child
            .raw_handle()
            .ok_or_else(|| ProcessError::Io("shell has no held process handle".into()))?;
        // SAFETY: creation transfers an unnamed, non-inheritable job handle.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(error("create payload job"));
        }
        let job = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut limits = ExtendedLimits::default();
        // KILL_ON_JOB_CLOSE, no breakaway.
        limits.basic.flags = 0x2000;
        // SAFETY: job and the configuration buffer remain valid for this call.
        if unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                9,
                (&limits as *const ExtendedLimits).cast(),
                std::mem::size_of::<ExtendedLimits>() as u32,
            )
        } == 0
        {
            return Err(error("set payload job kill-on-close"));
        }
        // The initial thread is still suspended: no child code can fork before
        // assignment. Windows 8+ nested-job restrictions fail closed.
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process) } == 0 {
            return Err(error("assign suspended shell to payload job"));
        }
        Ok(Self(job))
    }

    pub(super) fn terminate(&self) -> Result<(), ProcessError> {
        // SAFETY: the owned job handle names exactly this payload tree even
        // after the original shell PID has exited or has been recycled.
        if unsafe { TerminateJobObject(self.0.as_raw_handle(), 143) } == 0 {
            return Err(error("terminate payload job"));
        }
        Ok(())
    }

    /// A normally completed foreground shell may have deliberately detached
    /// descendants. Release our cancellation ownership without killing them.
    pub(super) fn disarm_on_close(&self) -> Result<(), ProcessError> {
        let limits = ExtendedLimits::default();
        // SAFETY: this payload-only job has no limits other than kill-on-close;
        // replacing that flag with zero leaves any live members running.
        if unsafe {
            SetInformationJobObject(
                self.0.as_raw_handle(),
                9,
                (&limits as *const ExtendedLimits).cast(),
                std::mem::size_of::<ExtendedLimits>() as u32,
            )
        } == 0
        {
            return Err(error("disarm completed foreground payload job"));
        }
        Ok(())
    }
}

pub(super) fn initialize() -> Result<(), ProcessError> {
    let _lock = INITIALIZE
        .lock()
        .map_err(|_| ProcessError::Io("supervisor job initialization poisoned".into()))?;
    if JOB.get().is_some() {
        return Ok(());
    }
    // Null SECURITY_ATTRIBUTES explicitly makes the unnamed job non-inheritable.
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() {
        return Err(error("create supervisor job"));
    }
    // SAFETY: the successful creation transfers unique handle ownership.
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut flags = 0;
    if unsafe { GetHandleInformation(job.as_raw_handle(), &mut flags) } == 0 || flags & 1 != 0 {
        return Err(error("supervisor job handle must not be inherited"));
    }
    let mut limits = ExtendedLimits::default();
    limits.basic.flags = 0x2000; // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, no breakaway.
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            9,
            (&limits as *const ExtendedLimits).cast(),
            std::mem::size_of::<ExtendedLimits>() as u32,
        )
    } == 0
    {
        return Err(error("set supervisor job kill-on-close"));
    }
    // Assign the supervisor, not its children. All future CreateProcess children
    // inherit this association atomically. Nested-job restrictions fail closed.
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), GetCurrentProcess()) } == 0 {
        return Err(error("assign supervisor job (Windows 8+ nested-job support and compatible host job required)"));
    }
    // INITIALIZE serializes every writer; after assignment we must never drop it.
    if let Err(job) = JOB.set(job) {
        std::mem::forget(job);
    }
    Ok(())
}

/// Hold the sole suspended initial thread before publishing the child's PID.
/// The live Child process handle prevents PID recycling during enumeration.
/// Unexpected extra threads are rejected; never guess which thread to resume.
pub(super) fn suspended_thread(child: &tokio::process::Child) -> Result<OwnedHandle, ProcessError> {
    let process = child
        .raw_handle()
        .ok_or_else(|| ProcessError::Io("supervised child has no process handle".into()))?;
    let pid = child
        .id()
        .ok_or_else(|| ProcessError::Io("supervised child has no PID".into()))?;
    if let Some(job) = JOB.get() {
        let mut member = 0;
        if unsafe { IsProcessInJob(process, job.as_raw_handle(), &mut member) } == 0 || member == 0
        {
            return Err(error("child did not inherit supervisor job"));
        }
    }
    let raw = unsafe { CreateToolhelp32Snapshot(4, 0) };
    if raw == -1isize as Handle {
        return Err(error("snapshot suspended child thread"));
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut entry = ThreadEntry {
        size: std::mem::size_of::<ThreadEntry>() as u32,
        ..Default::default()
    };
    let mut available = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) } != 0;
    let mut ids = Vec::new();
    while available {
        if entry.size >= 16 && entry.owner == pid {
            ids.push(entry.id);
        }
        entry.size = std::mem::size_of::<ThreadEntry>() as u32;
        available = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) } != 0;
    }
    if ids.len() != 1 {
        return Err(ProcessError::Io(format!(
            "suspended child must have exactly one initial thread, found {}",
            ids.len()
        )));
    }
    // THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION, non-inheritable.
    let raw = unsafe { OpenThread(2 | 0x0800, 0, ids[0]) };
    if raw.is_null() {
        return Err(error("open suspended initial thread"));
    }
    let thread = unsafe { OwnedHandle::from_raw_handle(raw) };
    if unsafe { GetProcessIdOfThread(thread.as_raw_handle()) } != pid {
        return Err(ProcessError::Io(
            "suspended initial thread identity changed".into(),
        ));
    }
    Ok(thread)
}

pub(super) fn release(thread: OwnedHandle) -> Result<(), ProcessError> {
    // Ownership of this exact thread handle was captured while the child was
    // still suspended. Do not look it up by PID/TID after capability acceptance.
    let previous = unsafe { ResumeThread(thread.as_raw_handle()) };
    if previous != 1 {
        return Err(ProcessError::Io(format!(
            "supervised initial thread resume expected suspension count 1, got {previous}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    const HELPER: &str = "process::supervisor_gate::tests::windows_job_gate_helper";
    const DETACHED_HELPER: &str =
        "process::supervisor_gate::tests::windows_foreground_detached_helper";

    #[test]
    fn windows_foreground_detached_helper() {
        let Some(directory) = std::env::var_os("LINGXI_DETACHED_JOB_TEST_DIR") else {
            return;
        };
        let directory = PathBuf::from(directory);
        if std::env::var_os("LINGXI_DETACHED_JOB_CHILD").is_some() {
            std::fs::write(directory.join("started"), "ready").unwrap();
            let until = std::time::Instant::now() + Duration::from_secs(30);
            while std::time::Instant::now() < until {
                if directory.join("release").exists() {
                    std::fs::write(directory.join("finished"), "survived").unwrap();
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            return;
        }
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([DETACHED_HELPER, "--exact", "--nocapture"])
            .env("LINGXI_DETACHED_JOB_CHILD", "1")
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        if std::env::var_os("LINGXI_DETACHED_JOB_HOLD_PIPE").is_none() {
            command.stdout(Stdio::null());
        }
        let _detached = command.spawn().unwrap();
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while !directory.join("started").exists() {
            assert!(
                std::time::Instant::now() < until,
                "descendant did not start"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[tokio::test]
    async fn normal_foreground_completion_preserves_detached_descendants() {
        use lingxi_core::host::sandbox::SandboxedTag;
        use lingxi_core::host::{ForegroundOutcome, SandboxedCommand};
        for hold_pipe in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut env = std::collections::HashMap::from([(
                "LINGXI_DETACHED_JOB_TEST_DIR".into(),
                directory.path().to_string_lossy().into_owned(),
            )]);
            if hold_pipe {
                env.insert("LINGXI_DETACHED_JOB_HOLD_PIPE".into(), "1".into());
            }
            let command = SandboxedCommand::__new_sandboxed(
                lingxi_core::host::sandbox::ProcessCommand {
                    command: std::env::current_exe()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    args: vec![
                        DETACHED_HELPER.into(),
                        "--exact".into(),
                        "--nocapture".into(),
                    ],
                    cwd: Some(directory.path().to_path_buf()),
                    env,
                    timeout: Some(Duration::from_secs(5)),
                    stdin: None,
                },
                SandboxedTag::BypassAuditedWithReason {
                    reason: "foreground job lifecycle regression".into(),
                },
            );
            let result = tokio::time::timeout(
                Duration::from_secs(15),
                super::super::background::run(&command, None, false),
            )
            .await;
            // Always release the bounded descendant before asserting, including
            // a failure in the foreground path under test.
            std::fs::write(directory.path().join("release"), "release").unwrap();
            let result = result.unwrap().unwrap();
            assert!(
                matches!(result.outcome, ForegroundOutcome::Completed(output) if output.exit_code == 0 && !output.timed_out)
            );
            await_file(&directory.path().join("finished")).await;
        }
    }

    #[tokio::test]
    async fn armed_payload_job_drop_still_terminates_its_child() {
        let mut command = tokio::process::Command::new("cmd.exe");
        command
            .args(["/D", "/S", "/C", "more"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(0x4)
            .kill_on_drop(true);
        let mut child = command.spawn().unwrap();
        let job = CommandJob::assign_suspended(&child).unwrap();
        release(suspended_thread(&child).unwrap()).unwrap();
        drop(job);
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
    }

    #[tokio::test]
    async fn windows_job_gate_helper() {
        let Some(directory) = std::env::var_os("LINGXI_GATE_TEST_DIR") else {
            return;
        };
        let directory = PathBuf::from(directory);
        initialize().unwrap();
        let mut command = tokio::process::Command::new("cmd.exe");
        command
            .args(["/D", "/S", "/C", "echo started>payload.started & more"])
            .current_dir(&directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(0x4)
            .kill_on_drop(true);
        let mut child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        let thread = suspended_thread(&child).unwrap();
        std::fs::write(directory.join("child.pid"), pid.to_string()).unwrap();
        // This is exactly the native interval occupied by on_spawn's remote ACK.
        tokio::time::timeout(Duration::from_secs(30), async {
            while !directory.join("accepted").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!directory.join("payload.started").exists());
        release(thread).unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(b"original stdin payload\r\n")
            .await
            .unwrap();
        drop(stdin);
        let output = child.wait_with_output().await.unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("original stdin payload"));
        std::fs::write(directory.join("finished"), pid.to_string()).unwrap();
    }

    async fn start_helper(directory: &std::path::Path) -> tokio::process::Child {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([HELPER, "--exact", "--nocapture"])
            .env("LINGXI_GATE_TEST_DIR", directory)
            .kill_on_drop(true);
        command.spawn().unwrap()
    }
    async fn await_file(path: &std::path::Path) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn windows_supervisor_death_before_cap_acceptance_kills_suspended_payload() {
        let directory = tempfile::tempdir().unwrap();
        let mut supervisor = start_helper(directory.path()).await;
        await_file(&directory.path().join("child.pid")).await;
        let pid: u32 = std::fs::read_to_string(directory.path().join("child.pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            super::super::supervisor_native::process_is_alive(pid),
            Some(true)
        );
        assert!(!directory.path().join("payload.started").exists());
        // Kill just the supervisor; no taskkill /T. Kernel job teardown alone
        // must kill the suspended child despite the source never accepting it.
        supervisor.kill().await.unwrap();
        supervisor.wait().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while super::super::supervisor_native::process_is_alive(pid) != Some(false) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(!directory.path().join("payload.started").exists());
    }
    #[tokio::test]
    async fn windows_cap_acceptance_releases_same_pid_with_original_stdin_and_arguments() {
        let directory = tempfile::tempdir().unwrap();
        let mut supervisor = start_helper(directory.path()).await;
        await_file(&directory.path().join("child.pid")).await;
        let original = std::fs::read_to_string(directory.path().join("child.pid")).unwrap();
        assert!(!directory.path().join("payload.started").exists());
        std::fs::write(directory.path().join("accepted"), "accepted").unwrap();
        await_file(&directory.path().join("finished")).await;
        assert_eq!(
            std::fs::read_to_string(directory.path().join("finished")).unwrap(),
            original
        );
        assert!(directory.path().join("payload.started").exists());
        tokio::time::timeout(Duration::from_secs(5), supervisor.wait())
            .await
            .unwrap()
            .unwrap();
    }
}
