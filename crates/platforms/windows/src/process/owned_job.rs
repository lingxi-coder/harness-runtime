//! Windows payload ownership for a dedicated, non-detaching helper process.
//!
//! Initialize the helper before accepting work. A payload is created suspended,
//! assigned to its own non-inheritable kill-on-close Job, and only then resumed
//! under the same gate used to accept cancellation. The payload Job is retained
//! after leader exit so the helper can still stop and verify all descendants.

use super::supervisor_gate::{self, CommandJob};
use mobile_linux_api::ProcessError;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::{Child, ChildStderr, ChildStdout, Command};
use tokio_util::sync::CancellationToken;

const CLEANUP_WAIT: Duration = Duration::from_secs(5);
static HELPER_INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Join this dedicated helper to an outer, non-inheritable kill-on-close Job.
/// Call only in the helper process: the membership lasts until process exit.
pub fn initialize_helper_job() -> Result<(), ProcessError> {
    supervisor_gate::protect_standard_handles()?;
    supervisor_gate::initialize()?;
    HELPER_INITIALIZED.store(true, Ordering::Release);
    Ok(())
}

/// Recheck whether the helper has a proof that all uncertain startup work
/// ended. The current Windows Job API cannot provide that proof without the
/// original process handles, so an initialized helper conservatively returns
/// `false`. This can keep a failed startup isolated indefinitely.
pub fn startup_group_empty_verified() -> Result<bool, ProcessError> {
    if !HELPER_INITIALIZED.load(Ordering::Acquire) || !supervisor_gate::initialized() {
        return Err(ProcessError::Io("helper Job is not initialized".into()));
    }
    Ok(false)
}

#[derive(Default)]
struct StartupState {
    cancelled: bool,
    may_have_resumed: bool,
}

/// Shared acceptance boundary for stop/EOF and the payload's initial resume.
/// Clone the gate into the control reader; `cancel` is synchronous so it can
/// record acceptance before that reader forwards any control message.
#[derive(Clone, Default)]
pub struct StartupGate {
    state: Arc<Mutex<StartupState>>,
    cancelled: CancellationToken,
}

impl StartupGate {
    /// Accept cancellation atomically against the initial thread's resume.
    pub fn cancel(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.cancelled = true;
        self.cancelled.cancel();
    }

    /// Whether cancellation has been accepted for this startup.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.is_cancelled()
    }

    /// Conservative resume provenance, including a failed ResumeThread
    /// attempt. This remains true if the spawn worker later unwinds.
    pub fn may_have_resumed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .may_have_resumed
    }

    /// Wait until cancellation is accepted; this wait is cancellation-safe.
    pub async fn cancelled(&self) {
        self.cancelled.cancelled().await;
    }
}

/// The original child and Job. Keep this value on every cleanup failure and
/// retry; dropping it requests kill-on-close but does not prove cleanup.
pub struct OwnedProcess {
    child: Child,
    job: Option<CommandJob>,
    // Set before the first ResumeThread attempt. A failed call cannot prove
    // that no payload instruction ran. Only false establishes that this
    // held, suspended leader could not have created a descendant.
    payload_may_have_run: bool,
}

impl std::fmt::Debug for OwnedProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedProcess")
            .field("job_assigned", &self.job.is_some())
            .finish_non_exhaustive()
    }
}

impl OwnedProcess {
    /// Take the payload's stdout pipe once; it is separate from helper control output.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    /// Take the payload's stderr pipe once; it is separate from helper control output.
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    /// Wait only for the leader. Tokio's child wait is cancellation-safe; the
    /// held Job continues to own any descendants after this future returns.
    pub async fn wait_leader(&mut self) -> Result<ExitStatus, ProcessError> {
        self.child.wait().await.map_err(io_error)
    }

    /// Request termination and prove cleanup only when a suspended leader
    /// was never resumed. A resumed payload may have untracked descendants,
    /// so even zero Job accounting and a reaped leader remain unverified.
    /// A cancelled future or an error leaves ownership in `self` for retry.
    /// Each attempt has bounded waits; successful return is cleanup proof, not
    /// proof that this particular signal caused an exit.
    pub async fn terminate_and_reap(&mut self) -> Result<ExitStatus, ProcessError> {
        self.terminate_and_reap_after_signal(std::future::ready(()))
            .await
    }

    async fn terminate_and_reap_after_signal(
        &mut self,
        after_signal: impl std::future::Future<Output = ()>,
    ) -> Result<ExitStatus, ProcessError> {
        let signal = match &self.job {
            Some(job) => job.terminate(),
            // Assignment failed while the sole initial thread was suspended;
            // therefore no payload code or descendants have run.
            None => self.child.start_kill().map_err(io_error),
        };
        after_signal.await;
        let waited = tokio::time::timeout(CLEANUP_WAIT, self.child.wait())
            .await
            .map_err(|_| ProcessError::Io("owned child wait timed out".into()))
            .and_then(|result| result.map_err(io_error));
        let group_proof = if self.payload_may_have_run {
            Err(ProcessError::Io(
                "owned cleanup unverified: no complete descendant handle proof".into(),
            ))
        } else {
            // Before any resume attempt the held leader is the only payload
            // process that could have run; its actual wait is checked below.
            Ok(())
        };
        match (waited, group_proof) {
            // A failed termination request does not defeat independent proof
            // that the never-resumed held child has actually exited.
            (Ok(status), Ok(())) => Ok(status),
            (waited, group_proof) => Err(ProcessError::Io(format!(
                "owned cleanup unverified: terminate={signal:?}; wait={waited:?}; group_proof={group_proof:?}"
            ))),
        }
    }
}

/// If a child was created, `owner` is always present, even for a failed
/// assignment, resume, or accepted pre-resume cancellation. The caller must
/// retain it and verify cleanup rather than discard an unverified process.
#[derive(Debug)]
pub struct OwnedSpawnError {
    /// The startup operation's error.
    pub error: ProcessError,
    /// Cancellation was accepted before the initial thread could be resumed.
    pub cancelled: bool,
    /// Retained child and Job, when creation succeeded; cleanup must be retried.
    pub owner: Option<OwnedProcess>,
    /// Whether an OS child may have been created. In particular, Tokio's
    /// spawn API does not promise that every error precedes child creation.
    pub may_have_spawned_child: bool,
    /// True only when startup has proven that no payload was created.
    pub spawn_group_empty_verified: bool,
}

impl std::fmt::Display for OwnedSpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl std::error::Error for OwnedSpawnError {}

fn io_error(error: std::io::Error) -> ProcessError {
    ProcessError::Io(error.to_string())
}

fn startup_error(
    error: ProcessError,
    cancelled: bool,
    owner: Option<OwnedProcess>,
) -> OwnedSpawnError {
    let may_have_spawned_child = owner.is_some();
    OwnedSpawnError {
        error,
        cancelled,
        owner,
        may_have_spawned_child,
        spawn_group_empty_verified: !may_have_spawned_child,
    }
}

/// Spawn synchronously; perform this on a blocking worker if control input
/// must keep being polled while Windows creates and assigns the payload.
/// The caller's environment (including an earlier `env_clear`) is preserved.
/// Stdin is null and stdout/stderr are separate pipes owned by the caller.
pub fn spawn_owned(command: Command, gate: &StartupGate) -> Result<OwnedProcess, OwnedSpawnError> {
    spawn_owned_inner(command, gate, || {}, || {}, |_| Ok(()))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StartupStage {
    AssignJob,
    OpenThread,
    ResumeThread,
}

fn spawn_owned_inner(
    mut command: Command,
    gate: &StartupGate,
    before_resume: impl FnOnce(),
    after_resume: impl FnOnce(),
    mut check_stage: impl FnMut(StartupStage) -> Result<(), ProcessError>,
) -> Result<OwnedProcess, OwnedSpawnError> {
    let cancelled = || ProcessError::Io("owned payload startup cancelled".into());
    if gate.is_cancelled() {
        return Err(startup_error(cancelled(), true, None));
    }
    if !HELPER_INITIALIZED.load(Ordering::Acquire) || !supervisor_gate::initialized() {
        return Err(startup_error(
            ProcessError::Io("helper Job must be initialized before payload spawn".into()),
            false,
            None,
        ));
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(0x0000_0004) // CREATE_SUSPENDED
        .kill_on_drop(true);
    let child = command.spawn().map_err(|error| OwnedSpawnError {
        error: io_error(error),
        cancelled: false,
        owner: None,
        // Tokio can return a post-spawn conversion error without handing the
        // StdChild back. Do not claim no child; the outer Job still owns it.
        may_have_spawned_child: true,
        spawn_group_empty_verified: startup_group_empty_verified().unwrap_or(false),
    })?;
    let mut owner = OwnedProcess {
        child,
        job: None,
        payload_may_have_run: false,
    };
    owner.job = match check_stage(StartupStage::AssignJob)
        .and_then(|()| CommandJob::assign_suspended(&owner.child))
    {
        Ok(job) => Some(job),
        Err(error) => return Err(startup_error(error, false, Some(owner))),
    };
    let thread = match check_stage(StartupStage::OpenThread)
        .and_then(|()| supervisor_gate::suspended_thread(&owner.child))
    {
        Ok(thread) => thread,
        Err(error) => return Err(startup_error(error, false, Some(owner))),
    };
    before_resume();
    let mut state = gate.state.lock().unwrap_or_else(|error| error.into_inner());
    if state.cancelled {
        return Err(startup_error(cancelled(), true, Some(owner)));
    }
    // Claim and resume while holding the same lock used by cancel. If cancel
    // won, payload code never runs; if resume won, subsequent cancel is a stop
    // of the already-owned Job and must be handled by the caller.
    if let Err(error) = check_stage(StartupStage::ResumeThread) {
        return Err(startup_error(error, false, Some(owner)));
    }
    // Record before calling Windows: even an error return cannot prove that
    // the initial thread never ran, and an unwind must preserve this fact.
    state.may_have_resumed = true;
    owner.payload_may_have_run = true;
    if let Err(error) = supervisor_gate::release(thread) {
        return Err(startup_error(error, false, Some(owner)));
    }
    after_resume();
    drop(state);
    Ok(owner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    const HELPER_TEST: &str = "process::owned_job::tests::owned_job_helper";
    const PAYLOAD_TEST: &str = "process::owned_job::tests::payload_fixture";

    fn payload(role: &str, dir: &std::path::Path) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([PAYLOAD_TEST, "--exact", "--nocapture"])
            .env("OWNED_JOB_PAYLOAD_ROLE", role)
            .env("OWNED_JOB_TEST_DIR", dir);
        command
    }

    #[test]
    fn payload_fixture() {
        let Some(role) = std::env::var_os("OWNED_JOB_PAYLOAD_ROLE") else {
            return;
        };
        let dir = std::path::PathBuf::from(std::env::var_os("OWNED_JOB_TEST_DIR").unwrap());
        match role.to_string_lossy().as_ref() {
            "marker" => std::fs::write(dir.join("ran"), "ran").unwrap(),
            "marked_slow" => {
                std::fs::write(dir.join("ran"), "ran").unwrap();
                std::thread::sleep(Duration::from_secs(60));
            }
            "io" => {
                let mut input = Vec::new();
                std::io::stdin().read_to_end(&mut input).unwrap();
                assert!(input.is_empty());
                assert_eq!(std::env::var("OWNED_JOB_KEEP").as_deref(), Ok("kept"));
                assert!(std::env::var_os("OWNED_JOB_SENTINEL").is_none());
                println!("owned-stdout-kept");
                eprintln!("owned-stderr-kept");
            }
            "descendant" | "slow" => {
                std::fs::write(dir.join("ready"), "ready").unwrap();
                std::thread::sleep(Duration::from_secs(60));
            }
            "leader" => {
                let child = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([PAYLOAD_TEST, "--exact", "--nocapture"])
                    .env("OWNED_JOB_PAYLOAD_ROLE", "descendant")
                    .env("OWNED_JOB_TEST_DIR", &dir)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap();
                std::fs::write(dir.join("descendant.pid"), child.id().to_string()).unwrap();
                let until = std::time::Instant::now() + Duration::from_secs(15);
                while !dir.join("ready").exists() {
                    assert!(
                        std::time::Instant::now() < until,
                        "descendant did not start"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            _ => panic!("unknown payload fixture role"),
        }
    }

    async fn wait_for_file(path: &std::path::Path) {
        tokio::time::timeout(Duration::from_secs(15), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fixture did not start");
    }

    async fn before_resume_cancel_is_linearized() {
        let dir = tempfile::tempdir().unwrap();
        let already_cancelled = StartupGate::default();
        already_cancelled.cancel();
        let early = spawn_owned(payload("marker", dir.path()), &already_cancelled).unwrap_err();
        assert!(
            early.cancelled
                && early.owner.is_none()
                && !early.may_have_spawned_child
                && early.spawn_group_empty_verified
        );
        let gate = StartupGate::default();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let gate_for_spawn = gate.clone();
        let command = payload("marker", dir.path());
        let spawn = tokio::task::spawn_blocking(move || {
            spawn_owned_inner(
                command,
                &gate_for_spawn,
                || {
                    ready_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(15)).unwrap();
                },
                || {},
                |_| Ok(()),
            )
        });
        ready_rx
            .recv_timeout(Duration::from_secs(15))
            .expect("spawn did not reach resume barrier");
        gate.cancel();
        release_tx.send(()).unwrap();
        let mut failure = spawn.await.unwrap().unwrap_err();
        assert!(failure.cancelled);
        assert!(failure.may_have_spawned_child);
        assert!(!failure.spawn_group_empty_verified);
        assert!(!gate.may_have_resumed());
        assert!(!dir.path().join("ran").exists());
        let mut owner = failure
            .owner
            .take()
            .expect("spawned suspended child must be retained");
        tokio::time::timeout(Duration::from_secs(15), owner.terminate_and_reap())
            .await
            .unwrap()
            .unwrap();
        assert!(!dir.path().join("ran").exists());
    }

    async fn resumed_then_cancelled_under_the_same_gate() {
        fn gate_is_locked(gate: &StartupGate) -> bool {
            match gate.state.try_lock() {
                Ok(_) => false,
                Err(std::sync::TryLockError::WouldBlock) => true,
                Err(std::sync::TryLockError::Poisoned(_)) => panic!("startup gate is poisoned"),
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let gate = StartupGate::default();
        // Negative control: an unlocked gate must not satisfy the check below.
        assert!(!gate_is_locked(&gate));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let gate_for_spawn = gate.clone();
        let command = payload("marked_slow", dir.path());
        let spawn = tokio::task::spawn_blocking(move || {
            spawn_owned_inner(
                command,
                &gate_for_spawn,
                || {},
                || {
                    ready_tx.send(()).unwrap();
                    // The payload readiness and cancel-worker handshakes each
                    // have their own deadlines before this barrier is released.
                    release_rx.recv_timeout(Duration::from_secs(30)).unwrap();
                },
                |_| Ok(()),
            )
        });
        ready_rx
            .recv_timeout(Duration::from_secs(15))
            .expect("spawn did not resume payload");
        wait_for_file(&dir.path().join("ran")).await;
        assert!(
            gate_is_locked(&gate),
            "the resume barrier must hold the cancellation gate"
        );
        let gate_for_cancel = gate.clone();
        let (cancel_started_tx, cancel_started_rx) = std::sync::mpsc::channel();
        let mut cancel = tokio::task::spawn_blocking(move || {
            cancel_started_tx.send(()).unwrap();
            gate_for_cancel.cancel();
        });
        cancel_started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("cancel worker did not start");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut cancel)
                .await
                .is_err(),
            "cancel cannot be accepted while the resume gate is held"
        );
        assert!(!gate.is_cancelled());
        release_tx.send(()).unwrap();
        let mut owner = spawn.await.unwrap().unwrap();
        cancel.await.unwrap();
        assert!(gate.is_cancelled());
        assert!(gate.may_have_resumed());
        tokio::time::timeout(Duration::from_secs(15), owner.terminate_and_reap())
            .await
            .unwrap()
            .unwrap();
    }

    async fn failed_spawn_stages_keep_the_exact_owner() {
        let dir = tempfile::tempdir().unwrap();
        let missing = Command::new(dir.path().join("nonexistent-payload.exe"));
        let error = spawn_owned(missing, &StartupGate::default()).unwrap_err();
        assert!(!error.cancelled && error.owner.is_none() && error.may_have_spawned_child);
        assert!(
            error.spawn_group_empty_verified,
            "outer Job contains only the helper"
        );
        let mut active = spawn_owned(payload("slow", dir.path()), &StartupGate::default()).unwrap();
        wait_for_file(&dir.path().join("ready")).await;
        let missing = Command::new(dir.path().join("nonexistent-payload.exe"));
        let busy = spawn_owned(missing, &StartupGate::default()).unwrap_err();
        assert!(
            busy.may_have_spawned_child && !busy.spawn_group_empty_verified,
            "an active payload prevents the group-empty proof"
        );
        assert!(!startup_group_empty_verified().unwrap());
        active.terminate_and_reap().await.unwrap();
        drop(active);
        assert!(startup_group_empty_verified().unwrap());
        for stage_to_fail in [
            StartupStage::AssignJob,
            StartupStage::OpenThread,
            StartupStage::ResumeThread,
        ] {
            let command = payload("marker", dir.path());
            let mut error = spawn_owned_inner(
                command,
                &StartupGate::default(),
                || {},
                || {},
                |stage| {
                    if stage == stage_to_fail {
                        Err(ProcessError::Io(
                            "injected Windows startup stage failure".into(),
                        ))
                    } else {
                        Ok(())
                    }
                },
            )
            .unwrap_err();
            assert!(!error.cancelled);
            assert!(error.may_have_spawned_child);
            assert!(!error.spawn_group_empty_verified);
            assert!(error
                .error
                .to_string()
                .contains("injected Windows startup stage failure"));
            let mut owner = error
                .owner
                .take()
                .expect("created child must survive in error owner");
            assert_eq!(
                owner.job.is_some(),
                stage_to_fail != StartupStage::AssignJob
            );
            assert!(!dir.path().join("ran").exists());
            tokio::time::timeout(Duration::from_secs(15), owner.terminate_and_reap())
                .await
                .unwrap()
                .unwrap();
            assert!(!dir.path().join("ran").exists());
        }
    }

    async fn unwind_preserves_resume_provenance() {
        for after_resume in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let gate = StartupGate::default();
            let worker_gate = gate.clone();
            let command = payload("marked_slow", dir.path());
            let worker = tokio::task::spawn_blocking(move || {
                spawn_owned_inner(
                    command,
                    &worker_gate,
                    || {
                        if !after_resume {
                            panic!("injected pre-resume worker failure");
                        }
                    },
                    || {
                        if after_resume {
                            panic!("injected post-resume worker failure");
                        }
                    },
                    |_| Ok(()),
                )
            });
            assert!(worker.await.is_err());
            assert_eq!(gate.may_have_resumed(), after_resume);
            tokio::time::timeout(Duration::from_secs(15), async {
                while !startup_group_empty_verified().unwrap() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("outer Job did not become empty after the worker failed");
            if !after_resume {
                assert!(!dir.path().join("ran").exists());
            }
        }
    }

    async fn independent_streams_and_environment() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = payload("io", dir.path());
        command
            .env_clear()
            .env("SystemRoot", std::env::var_os("SystemRoot").unwrap())
            .env("OWNED_JOB_PAYLOAD_ROLE", "io")
            .env("OWNED_JOB_TEST_DIR", dir.path())
            .env("OWNED_JOB_KEEP", "kept");
        let mut owner = spawn_owned(command, &StartupGate::default()).unwrap();
        let mut stdout = owner.take_stdout().expect("separate stdout");
        let mut stderr = owner.take_stderr().expect("separate stderr");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (read_out, read_err, waited) = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(
                stdout.read_to_end(&mut out),
                stderr.read_to_end(&mut err),
                owner.wait_leader()
            )
        })
        .await
        .expect("stdin/streams/leader did not settle");
        read_out.unwrap();
        read_err.unwrap();
        assert!(waited.unwrap().success());
        let out = String::from_utf8_lossy(&out);
        let err = String::from_utf8_lossy(&err);
        assert!(out.contains("owned-stdout-kept"));
        assert!(!out.contains("owned-stderr-kept"));
        assert!(err.contains("owned-stderr-kept"));
        assert!(!err.contains("owned-stdout-kept"));
        owner.terminate_and_reap().await.unwrap();
    }

    async fn leader_exit_does_not_release_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let mut owner =
            spawn_owned(payload("leader", dir.path()), &StartupGate::default()).unwrap();
        let status = tokio::time::timeout(Duration::from_secs(20), owner.wait_leader())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success());
        let pid: u32 = std::fs::read_to_string(dir.path().join("descendant.pid"))
            .unwrap()
            .parse()
            .unwrap();
        // Retain this exact process object before termination; a PID can be
        // reused after its last handle is closed.
        let descendant = super::super::supervisor_gate::hold_process_for_membership_test(pid)
            .expect("open descendant process");
        assert_eq!(
            super::super::supervisor_native::process_handle_is_alive_for_test(&descendant),
            Some(true)
        );
        let job = owner.job.as_ref().expect("payload job assigned");
        let active_before = job.active_processes().unwrap();
        assert!(
            job.contains_process(&descendant).unwrap(),
            "the held descendant must belong to this payload job before termination"
        );
        owner.terminate_and_reap().await.unwrap();
        if super::super::supervisor_native::process_handle_is_alive_for_test(&descendant)
            == Some(true)
        {
            let job = owner.job.as_ref().unwrap();
            let member_after = job.contains_process(&descendant);
            let active_after = job.active_processes();
            let start = std::time::Instant::now();
            let settled = tokio::time::timeout(Duration::from_secs(5), async {
                while super::super::supervisor_native::process_handle_is_alive_for_test(&descendant)
                    == Some(true)
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                super::super::supervisor_native::process_handle_is_alive_for_test(&descendant)
            })
            .await;
            panic!(
                "cleanup returned before descendant termination: active_before={active_before}, \
                 member_after={member_after:?}, active_after={active_after:?}, \
                 elapsed={:?}, eventual_state={settled:?}",
                start.elapsed()
            );
        }
        assert_eq!(
            super::super::supervisor_native::process_handle_is_alive_for_test(&descendant),
            Some(false)
        );
    }

    async fn resumed_job_remains_unverified_even_after_accounting_zero() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!startup_group_empty_verified().unwrap());
        let mut owner =
            spawn_owned(payload("leader", dir.path()), &StartupGate::default()).unwrap();
        assert!(owner.wait_leader().await.unwrap().success());
        let pid: u32 = std::fs::read_to_string(dir.path().join("descendant.pid"))
            .unwrap()
            .parse()
            .unwrap();
        let descendant = super::super::supervisor_gate::hold_process_for_membership_test(pid)
            .expect("open original descendant process");
        let job = owner.job.as_ref().expect("held payload job");
        assert!(job.contains_process(&descendant).unwrap());
        assert_eq!(
            super::super::supervisor_native::process_handle_is_alive_for_test(&descendant),
            Some(true)
        );
        let error = owner.terminate_and_reap().await.unwrap_err().to_string();
        assert!(
            error.contains("no complete descendant handle proof"),
            "{error}"
        );
        // The counter is deliberately queried only as a negative control. It
        // is not a receipt, even after this particular held handle signals.
        owner.job.as_ref().unwrap().wait_empty().await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match super::super::supervisor_native::process_handle_is_alive_for_test(&descendant)
                {
                    Some(false) => break,
                    Some(true) => tokio::time::sleep(Duration::from_millis(10)).await,
                    None => panic!("failed to inspect original descendant handle"),
                }
            }
        })
        .await
        .expect("held descendant never signaled");
        assert!(owner.terminate_and_reap().await.is_err());
        assert!(!startup_group_empty_verified().unwrap());
    }

    async fn cancelled_cleanup_can_be_retried() {
        let dir = tempfile::tempdir().unwrap();
        let gate = StartupGate::default();
        let mut owner = spawn_owned(payload("slow", dir.path()), &gate).unwrap();
        wait_for_file(&dir.path().join("ready")).await;
        gate.cancel();
        assert!(gate.is_cancelled());
        // Pause precisely after the termination request, rather than relying
        // on an external process handle to keep Job accounting nonzero.
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let mut cleanup = Box::pin(owner.terminate_and_reap_after_signal(async move {
            reached_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        }));
        tokio::select! {
            result = &mut cleanup => panic!("cleanup completed before cancellation: {result:?}"),
            reached = tokio::time::timeout(Duration::from_secs(5), reached_rx) => {
                reached.expect("cleanup did not reach the cancellation barrier").unwrap();
            }
        }
        drop(cleanup);
        tokio::time::timeout(Duration::from_secs(15), owner.terminate_and_reap())
            .await
            .unwrap()
            .unwrap();
    }

    async fn helper_crash_fixture() {
        let dir = std::path::PathBuf::from(std::env::var_os("OWNED_JOB_TEST_DIR").unwrap());
        let mut owner = spawn_owned(payload("leader", &dir), &StartupGate::default()).unwrap();
        let status = tokio::time::timeout(Duration::from_secs(20), owner.wait_leader())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success());
        // Isolate the outer helper Job: a nested payload Job with its own
        // kill-on-close flag would otherwise also kill the descendant.
        let inner = owner.job.take().unwrap();
        inner.disarm_on_close().unwrap();
        drop(inner);
        std::fs::write(dir.join("helper.ready"), "ready").unwrap();
        std::future::pending::<()>().await;
    }

    async fn standard_handle_isolation_fixture() {
        let dir = std::path::PathBuf::from(std::env::var_os("OWNED_JOB_TEST_DIR").unwrap());
        let owner = spawn_owned(payload("slow", &dir), &StartupGate::default()).unwrap();
        wait_for_file(&dir.join("ready")).await;
        std::fs::write(dir.join("owned.pid"), owner.child.id().unwrap().to_string()).unwrap();
        // Deliberately close all three helper-side control handles while the
        // payload remains alive. Parent EOF/broken-pipe proves none leaked.
        supervisor_gate::close_standard_handles_for_test().unwrap();
        std::fs::write(dir.join("helper.ready"), "ready").unwrap();
        std::future::pending::<()>().await;
        drop(owner);
    }

    #[test]
    fn owned_job_helper() {
        let Some(case) = std::env::var_os("OWNED_JOB_HELPER_CASE") else {
            return;
        };
        if case == "uninitialized" {
            assert!(startup_group_empty_verified().is_err());
            let dir = tempfile::tempdir().unwrap();
            let error =
                spawn_owned(payload("marker", dir.path()), &StartupGate::default()).unwrap_err();
            assert!(!error.cancelled && error.owner.is_none() && !error.may_have_spawned_child);
            assert!(error.spawn_group_empty_verified);
            return;
        }
        if case == "stdio" || case == "stdio_leak_control" {
            supervisor_gate::make_standard_handles_inheritable_for_test().unwrap();
        }
        initialize_helper_job().unwrap();
        if case == "stdio_leak_control" {
            // Positive control: undo protection before spawning this fixture so
            // the parent can prove its pipe probe detects a real leaked reader.
            supervisor_gate::make_standard_handles_inheritable_for_test().unwrap();
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            match case.to_string_lossy().as_ref() {
                "cancel" => before_resume_cancel_is_linearized().await,
                "resume_then_cancel" => resumed_then_cancelled_under_the_same_gate().await,
                "startup_errors" => failed_spawn_stages_keep_the_exact_owner().await,
                "unwind" => unwind_preserves_resume_provenance().await,
                "io" => independent_streams_and_environment().await,
                "descendant" => leader_exit_does_not_release_descendants().await,
                "unverified_descendant" => {
                    resumed_job_remains_unverified_even_after_accounting_zero().await
                }
                "retry" => cancelled_cleanup_can_be_retried().await,
                "crash" => helper_crash_fixture().await,
                "stdio" | "stdio_leak_control" => standard_handle_isolation_fixture().await,
                _ => panic!("unknown owned job helper case"),
            }
        });
    }

    #[tokio::test]
    async fn job_unknown_and_suspended_start_are_proven_in_dedicated_helpers() {
        for case in ["unverified_descendant", "cancel"] {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([HELPER_TEST, "--exact", "--nocapture"])
                .env("OWNED_JOB_HELPER_CASE", case)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let output = tokio::time::timeout(Duration::from_secs(45), command.output())
                .await
                .unwrap_or_else(|_| panic!("helper timed out: {case}"))
                .unwrap();
            assert!(
                output.status.success(),
                "helper {case} failed: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[tokio::test]
    async fn owned_job_native_regressions_in_dedicated_helpers() {
        for case in [
            "uninitialized",
            "cancel",
            "resume_then_cancel",
            "startup_errors",
            "unwind",
            "io",
            "descendant",
            "retry",
        ] {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([HELPER_TEST, "--exact", "--nocapture"])
                .env("OWNED_JOB_HELPER_CASE", case)
                .env("OWNED_JOB_SENTINEL", "must-not-be-inherited")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let output = tokio::time::timeout(Duration::from_secs(45), command.output())
                .await
                .unwrap_or_else(|_| panic!("helper timed out: {case}"))
                .unwrap();
            assert!(
                output.status.success(),
                "helper {case} failed: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[tokio::test]
    async fn outer_job_kills_descendant_after_helper_crash() {
        let dir = tempfile::tempdir().unwrap();
        let mut helper = Command::new(std::env::current_exe().unwrap())
            .args([HELPER_TEST, "--exact", "--nocapture"])
            .env("OWNED_JOB_HELPER_CASE", "crash")
            .env("OWNED_JOB_TEST_DIR", dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        wait_for_file(&dir.path().join("helper.ready")).await;
        let pid: u32 = std::fs::read_to_string(dir.path().join("descendant.pid"))
            .unwrap()
            .parse()
            .unwrap();
        // Hold the process object so PID reuse cannot disguise a surviving
        // descendant after the helper has been terminated.
        let held = super::super::supervisor_native::hold_process_handle_for_test(pid).unwrap();
        assert_eq!(
            super::super::supervisor_native::process_is_alive(pid),
            Some(true)
        );
        // Kill only the helper; do not call taskkill /T or the payload Job.
        helper.kill().await.unwrap();
        helper.wait().await.unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            while super::super::supervisor_native::process_is_alive(pid) != Some(false) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("outer Job failed to stop the live descendant on helper death");
        drop(held);
    }

    #[tokio::test]
    async fn payload_does_not_inherit_helper_standard_control_handles() {
        for (case, expect_leaked_input) in [("stdio", false), ("stdio_leak_control", true)] {
            let dir = tempfile::tempdir().unwrap();
            let mut helper = Command::new(std::env::current_exe().unwrap())
                .args([HELPER_TEST, "--exact", "--nocapture"])
                .env("OWNED_JOB_HELPER_CASE", case)
                .env("OWNED_JOB_TEST_DIR", dir.path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let mut input = helper.stdin.take().unwrap();
            let mut stdout = helper.stdout.take().unwrap();
            let mut stderr = helper.stderr.take().unwrap();
            wait_for_file(&dir.path().join("helper.ready")).await;
            let pid: u32 = std::fs::read_to_string(dir.path().join("owned.pid"))
                .unwrap()
                .parse()
                .unwrap();
            let held = super::super::supervisor_native::hold_process_handle_for_test(pid).unwrap();
            assert_eq!(
                super::super::supervisor_native::process_handle_is_alive_for_test(&held),
                Some(true)
            );
            if !expect_leaked_input {
                let mut out = Vec::new();
                let mut err = Vec::new();
                let (out_result, err_result) =
                    tokio::time::timeout(Duration::from_secs(5), async {
                        tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err))
                    })
                    .await
                    .expect("payload inherited a helper output control handle");
                out_result.unwrap();
                err_result.unwrap();
            }
            // On Windows Tokio buffers ChildStdin writes on a blocking worker:
            // write_all can succeed before WriteFile observes a broken pipe.
            let write_result = tokio::time::timeout(Duration::from_secs(5), async {
                input.write_all(b"x").await?;
                input.flush().await
            })
            .await
            .expect("helper input write or flush hung");
            assert_eq!(
                write_result.is_ok(),
                expect_leaked_input,
                "unexpected helper input inheritance for {case}: {write_result:?}"
            );
            assert!(helper.try_wait().unwrap().is_none());
            assert_eq!(
                super::super::supervisor_native::process_handle_is_alive_for_test(&held),
                Some(true)
            );
            helper.kill().await.unwrap();
            helper.wait().await.unwrap();
            tokio::time::timeout(Duration::from_secs(15), async {
                while super::super::supervisor_native::process_handle_is_alive_for_test(&held)
                    != Some(false)
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("payload survived isolated helper termination");
        }
    }
}
