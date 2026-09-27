use core::fmt;
use std::io;
#[cfg(unix)]
use std::os::fd::RawFd;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use anyhow::anyhow;
use anyhow::Context as _;
use portable_pty::MasterPty;
use portable_pty::PtySize;
use portable_pty::SlavePty;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;

/// Signals supported by PTY process handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessSignal {
    /// Request an interactive interrupt (`SIGINT` on Unix).
    Interrupt,
    /// Request graceful process-tree termination (`SIGTERM` on Unix).
    Terminate,
    /// Force process-tree termination (`SIGKILL` on Unix).
    Kill,
}

pub(crate) fn unsupported_signal(signal: ProcessSignal) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        format!("process signal {signal:?} is not supported by this PTY backend"),
    )
}

pub(crate) trait ChildTerminator: Send + Sync {
    fn signal(&mut self, signal: ProcessSignal) -> io::Result<()>;

    fn kill(&mut self) -> io::Result<()>;
}

/// Terminal dimensions in character cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalSize {
    /// Number of terminal rows.
    pub rows: u16,
    /// Number of terminal columns.
    pub cols: u16,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self { rows: 24, cols: 80 }
    }
}

impl TryFrom<TerminalSize> for PtySize {
    type Error = anyhow::Error;

    fn try_from(value: TerminalSize) -> Result<Self, Self::Error> {
        if value.rows == 0 || value.cols == 0 {
            return Err(anyhow!("PTY dimensions must both be greater than zero"));
        }
        Ok(Self {
            rows: value.rows,
            cols: value.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
    }
}

#[cfg(unix)]
pub(crate) trait PtyHandleKeepAlive: Send {}

#[cfg(unix)]
impl<T: Send + ?Sized> PtyHandleKeepAlive for T {}

pub(crate) enum PtyMasterHandle {
    Resizable(Box<dyn MasterPty + Send>),
    #[cfg(unix)]
    Opaque {
        raw_fd: RawFd,
        _handle: Box<dyn PtyHandleKeepAlive>,
    },
}

pub(crate) struct PtyHandles {
    // The ConPTY slave owns state needed for the lifetime of the pseudoconsole.
    pub(crate) _slave: Option<Box<dyn SlavePty + Send>>,
    pub(crate) master: PtyMasterHandle,
}

impl fmt::Debug for PtyHandles {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("PtyHandles").finish_non_exhaustive()
    }
}

/// Handle used to drive and control a PTY child.
pub struct ProcessHandle {
    writer_tx: StdMutex<Option<mpsc::Sender<Vec<u8>>>>,
    terminator: StdMutex<Option<Box<dyn ChildTerminator>>>,
    writer_handle: StdMutex<Option<JoinHandle<()>>>,
    exit_status: Arc<AtomicBool>,
    exit_code: Arc<StdMutex<Option<i32>>>,
    exit_watch: watch::Receiver<Option<i32>>,
    pty_handles: StdMutex<Option<PtyHandles>>,
    process_id: Option<u32>,
    process_group_id: Option<u32>,
}

impl fmt::Debug for ProcessHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessHandle")
            .field("process_id", &self.process_id)
            .field("process_group_id", &self.process_group_id)
            .field("has_exited", &self.has_exited())
            .finish_non_exhaustive()
    }
}

impl ProcessHandle {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        writer_tx: mpsc::Sender<Vec<u8>>,
        terminator: Box<dyn ChildTerminator>,
        writer_handle: JoinHandle<()>,
        exit_status: Arc<AtomicBool>,
        exit_code: Arc<StdMutex<Option<i32>>>,
        exit_watch: watch::Receiver<Option<i32>>,
        pty_handles: PtyHandles,
        process_id: Option<u32>,
        process_group_id: Option<u32>,
    ) -> Self {
        Self {
            writer_tx: StdMutex::new(Some(writer_tx)),
            terminator: StdMutex::new(Some(terminator)),
            writer_handle: StdMutex::new(Some(writer_handle)),
            exit_status,
            exit_code,
            exit_watch,
            pty_handles: StdMutex::new(Some(pty_handles)),
            process_id,
            process_group_id,
        }
    }

    /// Return a bounded sender for writing raw bytes to the PTY.
    pub fn writer_sender(&self) -> mpsc::Sender<Vec<u8>> {
        if let Ok(writer_tx) = self.writer_tx.lock() {
            if let Some(writer_tx) = writer_tx.as_ref() {
                return writer_tx.clone();
            }
        }

        let (writer_tx, writer_rx) = mpsc::channel(1);
        drop(writer_rx);
        writer_tx
    }

    /// Write raw bytes to the PTY, applying bounded backpressure.
    pub async fn write(&self, bytes: Vec<u8>) -> anyhow::Result<()> {
        let writer = self
            .writer_tx
            .lock()
            .map_err(|_| anyhow!("failed to lock PTY writer"))?
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("PTY stdin is closed"))?;
        writer
            .send(bytes)
            .await
            .map_err(|_| anyhow!("PTY child is no longer accepting input"))
    }

    /// Resize the PTY in character cells.
    pub fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        let size = PtySize::try_from(size)?;
        let handles = self
            .pty_handles
            .lock()
            .map_err(|_| anyhow!("failed to lock PTY handles"))?;
        let handles = handles
            .as_ref()
            .ok_or_else(|| anyhow!("PTY is already closed"))?;
        match &handles.master {
            PtyMasterHandle::Resizable(master) => {
                master.resize(size).context("failed to resize PTY")
            }
            #[cfg(unix)]
            PtyMasterHandle::Opaque { raw_fd, .. } => resize_raw_pty(*raw_fd, size),
        }
    }

    /// Close the PTY input channel. This does not terminate the child.
    pub fn close_stdin(&self) {
        if let Ok(mut writer_tx) = self.writer_tx.lock() {
            writer_tx.take();
        }
    }

    /// Return true after the child wait task has observed exit.
    pub fn has_exited(&self) -> bool {
        self.exit_status.load(Ordering::SeqCst)
    }

    /// Return the child exit code when known.
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code.lock().ok().and_then(|guard| *guard)
    }

    /// Wait for the child to exit. Multiple callers may wait concurrently.
    pub async fn wait(&self) -> i32 {
        if let Some(code) = self.exit_code() {
            return code;
        }

        let mut exit_watch = self.exit_watch.clone();
        loop {
            if let Some(code) = *exit_watch.borrow_and_update() {
                return code;
            }
            if exit_watch.changed().await.is_err() {
                return self.exit_code().unwrap_or(-1);
            }
        }
    }

    /// Return the direct child process identifier when the backend exposes it.
    pub fn process_id(&self) -> Option<u32> {
        self.process_id
    }

    /// Return the process-group identifier used for whole-tree signals.
    ///
    /// On Unix the PTY child is a session and process-group leader. Windows
    /// uses a Job Object and therefore has no numeric group identifier.
    pub fn process_group_id(&self) -> Option<u32> {
        self.process_group_id
    }

    /// Send a signal to the child process tree.
    pub fn signal(&self, signal: ProcessSignal) -> io::Result<()> {
        let mut terminator = self
            .terminator
            .lock()
            .map_err(|_| io::Error::other("failed to lock PTY terminator"))?;
        match terminator.as_mut() {
            Some(terminator) => terminator.signal(signal),
            None => Ok(()),
        }
    }

    /// Force termination while leaving output/wait tasks alive to drain EOF.
    pub fn request_terminate(&self) {
        if let Ok(mut terminator) = self.terminator.lock() {
            if let Some(mut terminator) = terminator.take() {
                let _ = terminator.kill();
            }
        }
    }

    /// Terminate the complete child process tree and close its input.
    ///
    /// Output and wait tasks are deliberately not aborted: callers may keep
    /// draining `stdout_rx` until EOF and still observe the final exit code.
    pub fn terminate(&self) {
        self.request_terminate();
        self.close_stdin();
    }
}

#[cfg(unix)]
fn resize_raw_pty(raw_fd: RawFd, size: PtySize) -> anyhow::Result<()> {
    let size = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: size.pixel_width,
        ws_ypixel: size.pixel_height,
    };
    let result = unsafe { libc::ioctl(raw_fd, libc::TIOCSWINSZ, &size) };
    if result == -1 {
        return Err(io::Error::last_os_error()).context("failed to resize PTY");
    }
    Ok(())
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        self.terminate();
        if let Ok(mut writer_handle) = self.writer_handle.lock() {
            if let Some(writer_handle) = writer_handle.take() {
                writer_handle.abort();
            }
        }
        // Dropping the PTY owner closes the master after the child tree has
        // been terminated. The detached reader/wait tasks then finish on EOF.
        if let Ok(mut pty_handles) = self.pty_handles.lock() {
            pty_handles.take();
        }
    }
}

/// A spawned PTY process and its bounded I/O/exit receivers.
#[derive(Debug)]
pub struct SpawnedProcess {
    /// Interactive process controller.
    pub session: ProcessHandle,
    /// PTY's merged stdout/stderr byte stream.
    pub stdout_rx: mpsc::Receiver<Vec<u8>>,
    /// Empty receiver retained for compatibility with split pipe backends.
    pub stderr_rx: mpsc::Receiver<Vec<u8>>,
    /// One-shot child exit code notification.
    pub exit_rx: oneshot::Receiver<i32>,
}
