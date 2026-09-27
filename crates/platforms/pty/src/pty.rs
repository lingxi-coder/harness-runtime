use std::collections::HashMap;
#[cfg(unix)]
use std::fs::File;
use std::io::ErrorKind;
use std::io::Read as _;
use std::io::Write as _;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::fd::FromRawFd;
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::Path;
#[cfg(unix)]
use std::process::Command as StdCommand;
#[cfg(unix)]
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt as _;

use anyhow::Context as _;
use anyhow::Result;
#[cfg(not(windows))]
use portable_pty::native_pty_system;
use portable_pty::CommandBuilder;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;

use crate::process::ChildTerminator;
use crate::process::ProcessHandle;
use crate::process::ProcessSignal;
use crate::process::PtyHandles;
use crate::process::PtyMasterHandle;
use crate::process::SpawnedProcess;
use crate::process::TerminalSize;

const IO_CHANNEL_CAPACITY: usize = 128;
const READ_BUFFER_SIZE: usize = 8 * 1024;

/// Return whether the native platform PTY is available.
///
/// Unix PTYs are always available at the API level. On Windows,
/// `portable-pty` loads `ConPTY` dynamically during `openpty`; Windows versions
/// older than Windows 10 1809 will return an error from spawn.
#[must_use]
pub fn conpty_supported() -> bool {
    #[cfg(windows)]
    {
        crate::win::conpty_supported()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

struct PtyChildTerminator {
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    #[cfg(unix)]
    process_group_id: Option<u32>,
}

impl PtyChildTerminator {
    fn hard_kill(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        if let Some(process_group_id) = self.process_group_id {
            let group_result = crate::process_group::kill_process_group(process_group_id);
            let child_result = self.killer.kill();
            return match child_result {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == ErrorKind::NotFound => group_result,
                Err(error) => group_result.or(Err(error)),
            };
        }

        #[allow(unreachable_code)]
        self.killer.kill()
    }
}

impl ChildTerminator for PtyChildTerminator {
    fn signal(&mut self, signal: ProcessSignal) -> std::io::Result<()> {
        match signal {
            ProcessSignal::Interrupt => {
                #[cfg(unix)]
                if let Some(process_group_id) = self.process_group_id {
                    return crate::process_group::interrupt_process_group(process_group_id);
                }
                Err(crate::process::unsupported_signal(signal))
            }
            ProcessSignal::Terminate => {
                #[cfg(unix)]
                if let Some(process_group_id) = self.process_group_id {
                    return crate::process_group::terminate_process_group(process_group_id)
                        .map(|_| ());
                }
                self.hard_kill()
            }
            ProcessSignal::Kill => self.hard_kill(),
        }
    }

    fn kill(&mut self) -> std::io::Result<()> {
        self.hard_kill()
    }
}

#[cfg(unix)]
struct RawPidTerminator {
    process_group_id: u32,
}

#[cfg(unix)]
impl ChildTerminator for RawPidTerminator {
    fn signal(&mut self, signal: ProcessSignal) -> std::io::Result<()> {
        match signal {
            ProcessSignal::Interrupt => {
                crate::process_group::interrupt_process_group(self.process_group_id)
            }
            ProcessSignal::Terminate => {
                crate::process_group::terminate_process_group(self.process_group_id).map(|_| ())
            }
            ProcessSignal::Kill => crate::process_group::kill_process_group(self.process_group_id),
        }
    }

    fn kill(&mut self) -> std::io::Result<()> {
        crate::process_group::kill_process_group(self.process_group_id)
    }
}

/// Spawn a process attached to a controlling pseudo-terminal.
///
/// The environment is cleared before `env` is applied. On Unix,
/// `inherited_fds` remain open across exec; all other non-stdio descriptors
/// without `FD_CLOEXEC` are closed. Windows does not support inherited FDs
/// through this API.
#[allow(
    clippy::implicit_hasher,
    clippy::similar_names,
    clippy::too_many_arguments,
    clippy::unused_async
)]
pub async fn spawn_process(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
    size: TerminalSize,
    inherited_fds: &[i32],
) -> Result<SpawnedProcess> {
    if program.is_empty() {
        anyhow::bail!("missing program for PTY spawn");
    }
    if !cwd.is_dir() {
        anyhow::bail!("PTY working directory does not exist: {}", cwd.display());
    }
    #[cfg(windows)]
    if !conpty_supported() {
        anyhow::bail!("ConPTY requires Windows 10 build 17763 or newer");
    }
    let _ = portable_pty::PtySize::try_from(size)?;

    #[cfg(unix)]
    if !inherited_fds.is_empty() {
        return spawn_process_preserving_fds(program, args, cwd, env, arg0, size, inherited_fds)
            .await;
    }

    #[cfg(not(unix))]
    if !inherited_fds.is_empty() {
        anyhow::bail!("inherited PTY file descriptors are only supported on Unix");
    }

    spawn_process_portable(program, args, cwd, env, arg0, size).await
}

fn platform_native_pty_system() -> Box<dyn portable_pty::PtySystem + Send> {
    #[cfg(windows)]
    {
        Box::new(crate::win::ConPtySystem::default())
    }

    #[cfg(not(windows))]
    {
        native_pty_system()
    }
}

#[allow(clippy::similar_names, clippy::unused_async)]
async fn spawn_process_portable(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
    size: TerminalSize,
) -> Result<SpawnedProcess> {
    let pty_system = platform_native_pty_system();
    let pair = pty_system.openpty(portable_pty::PtySize::try_from(size)?)?;

    // Match Codex's API contract: when present, arg0 is the executable name
    // supplied to portable-pty. Most callers should pass None.
    let command_name = arg0.as_deref().unwrap_or(program);
    let mut command = CommandBuilder::new(command_name);
    command.cwd(cwd);
    command.env_clear();
    command.args(args);
    for (key, value) in env {
        command.env(key, value);
    }

    let mut child = pair
        .slave
        .spawn_command(command)
        .with_context(|| format!("failed to spawn PTY command {program:?}"))?;
    let process_id = child.process_id();
    #[cfg(unix)]
    let process_group_id = process_id;
    let killer = child.clone_killer();

    let reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    let (writer_tx, writer_rx) = mpsc::channel(IO_CHANNEL_CAPACITY);
    let (stdout_tx, stdout_rx) = mpsc::channel(IO_CHANNEL_CAPACITY);
    let (_stderr_tx, stderr_rx) = mpsc::channel(1);

    spawn_reader_task(reader, stdout_tx);
    let writer_handle = spawn_writer_task(writer, writer_rx);

    let (exit_tx, exit_rx) = oneshot::channel();
    let (exit_watch_tx, exit_watch_rx) = watch::channel(None);
    let exit_status = Arc::new(AtomicBool::new(false));
    let wait_exit_status = Arc::clone(&exit_status);
    let exit_code = Arc::new(StdMutex::new(None));
    let wait_exit_code = Arc::clone(&exit_code);
    tokio::task::spawn_blocking(move || {
        let code = child
            .wait()
            .map(|status| i32::try_from(status.exit_code()).unwrap_or(-1))
            .unwrap_or(-1);
        publish_exit(
            code,
            &wait_exit_status,
            &wait_exit_code,
            &exit_watch_tx,
            exit_tx,
        );
    });

    let handles = PtyHandles {
        _slave: if cfg!(windows) {
            Some(pair.slave)
        } else {
            None
        },
        master: PtyMasterHandle::Resizable(pair.master),
    };
    let session = ProcessHandle::new(
        writer_tx,
        Box::new(PtyChildTerminator {
            killer,
            #[cfg(unix)]
            process_group_id,
        }),
        writer_handle,
        exit_status,
        exit_code,
        exit_watch_rx,
        handles,
        process_id,
        #[cfg(unix)]
        process_group_id,
        #[cfg(not(unix))]
        None,
    );

    Ok(SpawnedProcess {
        session,
        stdout_rx,
        stderr_rx,
        exit_rx,
    })
}

fn spawn_reader_task(mut reader: Box<dyn std::io::Read + Send>, stdout_tx: mpsc::Sender<Vec<u8>>) {
    tokio::task::spawn_blocking(move || {
        let mut buffer = [0_u8; READ_BUFFER_SIZE];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if stdout_tx.blocking_send(buffer[..read].to_vec()).is_err() {
                        break;
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
}

fn spawn_writer_task(
    writer: Box<dyn std::io::Write + Send>,
    mut writer_rx: mpsc::Receiver<Vec<u8>>,
) -> tokio::task::JoinHandle<()> {
    let writer = Arc::new(tokio::sync::Mutex::new(writer));
    tokio::spawn(async move {
        #[cfg(windows)]
        let mut windows_input = crate::windows_input::WindowsTtyInputNormalizer::default();
        while let Some(bytes) = writer_rx.recv().await {
            #[cfg(windows)]
            let bytes = windows_input.normalize(&bytes);
            let mut writer = writer.lock().await;
            if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                break;
            }
        }
    })
}

fn publish_exit(
    code: i32,
    exit_status: &AtomicBool,
    exit_code: &StdMutex<Option<i32>>,
    exit_watch_tx: &watch::Sender<Option<i32>>,
    exit_tx: oneshot::Sender<i32>,
) {
    exit_status.store(true, Ordering::SeqCst);
    if let Ok(mut stored_code) = exit_code.lock() {
        *stored_code = Some(code);
    }
    let _ = exit_watch_tx.send(Some(code));
    let _ = exit_tx.send(code);
}

#[cfg(unix)]
#[allow(
    clippy::similar_names,
    clippy::too_many_arguments,
    clippy::unused_async
)]
async fn spawn_process_preserving_fds(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &HashMap<String, String>,
    arg0: &Option<String>,
    size: TerminalSize,
    inherited_fds: &[RawFd],
) -> Result<SpawnedProcess> {
    let (master, slave) = open_unix_pty(size)?;
    let mut command = StdCommand::new(program);
    if let Some(arg0) = arg0 {
        command.arg0(arg0);
    }
    command.current_dir(cwd);
    command.env_clear();
    command.args(args);
    command.envs(env);

    let stdin = slave.try_clone()?;
    let stdout = slave.try_clone()?;
    let stderr = slave.try_clone()?;
    let inherited_fds = inherited_fds.to_vec();
    unsafe {
        command
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .pre_exec(move || {
                reset_child_signals();
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                #[allow(clippy::cast_lossless)]
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                close_inherited_fds_except(&inherited_fds);
                Ok(())
            });
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn PTY command {program:?}"))?;
    drop(slave);
    let process_id = Some(child.id());
    let process_group_id = child.id();

    let reader = Box::new(master.try_clone()?) as Box<dyn std::io::Read + Send>;
    let writer = Box::new(master.try_clone()?) as Box<dyn std::io::Write + Send>;
    let (writer_tx, writer_rx) = mpsc::channel(IO_CHANNEL_CAPACITY);
    let (stdout_tx, stdout_rx) = mpsc::channel(IO_CHANNEL_CAPACITY);
    let (_stderr_tx, stderr_rx) = mpsc::channel(1);
    spawn_reader_task(reader, stdout_tx);
    let writer_handle = spawn_writer_task(writer, writer_rx);

    let (exit_tx, exit_rx) = oneshot::channel();
    let (exit_watch_tx, exit_watch_rx) = watch::channel(None);
    let exit_status = Arc::new(AtomicBool::new(false));
    let wait_exit_status = Arc::clone(&exit_status);
    let exit_code = Arc::new(StdMutex::new(None));
    let wait_exit_code = Arc::clone(&exit_code);
    tokio::task::spawn_blocking(move || {
        let code = child.wait().map(exit_code_from_status).unwrap_or(-1);
        publish_exit(
            code,
            &wait_exit_status,
            &wait_exit_code,
            &exit_watch_tx,
            exit_tx,
        );
    });

    let raw_fd = master.as_raw_fd();
    let handles = PtyHandles {
        _slave: None,
        master: PtyMasterHandle::Opaque {
            raw_fd,
            _handle: Box::new(master),
        },
    };
    let session = ProcessHandle::new(
        writer_tx,
        Box::new(RawPidTerminator { process_group_id }),
        writer_handle,
        exit_status,
        exit_code,
        exit_watch_rx,
        handles,
        process_id,
        Some(process_group_id),
    );

    Ok(SpawnedProcess {
        session,
        stdout_rx,
        stderr_rx,
        exit_rx,
    })
}

#[cfg(unix)]
fn open_unix_pty(size: TerminalSize) -> Result<(File, File)> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let mut size = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(size),
        )
    };
    if result != 0 {
        anyhow::bail!("failed to open PTY: {}", std::io::Error::last_os_error());
    }
    if let Err(error) = set_cloexec(master).and_then(|()| set_cloexec(slave)) {
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        return Err(error.into());
    }
    Ok(unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) })
}

#[cfg(unix)]
fn set_cloexec(fd: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn reset_child_signals() {
    for signal in [
        libc::SIGCHLD,
        libc::SIGHUP,
        libc::SIGINT,
        libc::SIGQUIT,
        libc::SIGTERM,
        libc::SIGALRM,
    ] {
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
        }
    }
    let empty_set: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigprocmask(libc::SIG_SETMASK, &empty_set, std::ptr::null_mut());
    }
}

#[cfg(unix)]
fn close_inherited_fds_except(preserved_fds: &[RawFd]) {
    let Ok(directory) = std::fs::read_dir("/dev/fd") else {
        return;
    };
    let mut close_fds = Vec::new();
    for entry in directory.flatten() {
        let Some(fd) = entry
            .file_name()
            .into_string()
            .ok()
            .and_then(|name| name.parse::<RawFd>().ok())
        else {
            continue;
        };
        if fd <= 2 || preserved_fds.contains(&fd) {
            continue;
        }
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        // Preserve CLOEXEC descriptors, including std::process's exec-error
        // pipe, so spawn failures are reported to the parent.
        if flags != -1 && flags & libc::FD_CLOEXEC == 0 {
            close_fds.push(fd);
        }
    }
    for fd in close_fds {
        unsafe {
            libc::close(fd);
        }
    }
}

#[cfg(unix)]
fn exit_code_from_status(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    status.signal().map_or(-1, |signal| 128 + signal)
}
