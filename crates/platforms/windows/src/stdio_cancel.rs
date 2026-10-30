//! Cancellable, owned Windows stdio for a single protocol connection.
//!
//! Anonymous Windows pipes cannot be converted to overlapped I/O after they
//! are inherited. Each direction therefore owns a dedicated synchronous I/O
//! thread. Cancellation requests target only those threads and are retried
//! until the threads actually exit; a successful cancellation request is not a
//! completion receipt. The pipe's original mode is never changed.
//!
//! A pending `AsyncWrite` may have written bytes before its future is dropped.
//! The caller must abandon the entire byte stream after cancelling such a
//! write, rather than attempting another frame on it. `shutdown` reports
//! whether both workers were joined; `last_stdout_completion` exposes the
//! actual result of the most recently completed write attempt.
#![allow(unsafe_code)]

use std::future::Future;
use std::io::{self, Read, Write};
use std::os::windows::io::{AsHandle, AsRawHandle, OwnedHandle};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::task::{Context, Poll};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use windows_sys::Win32::Foundation::{
    ERROR_NOT_FOUND, ERROR_OPERATION_ABORTED, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
use windows_sys::Win32::System::Threading::WaitForSingleObject;
use windows_sys::Win32::System::IO::CancelSynchronousIo;

const RETRY_INTERVAL: Duration = Duration::from_millis(10);
const READ_SIZE: usize = 8192;
const READ_QUEUE: usize = 8;
const BETWEEN_IO: u8 = 0;
const ENTERING_IO: u8 = 1;
const EXITED: u8 = 2;

/// The latest stdout write state. Pending is not an OS completion, and an
/// error does not establish that no bytes reached the peer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StdoutCompletion {
    /// A write was submitted, but its OS completion has not been observed yet.
    Pending,
    /// Shutdown won before the submitted operation entered the OS write call.
    NotStarted,
    /// The operating system accepted this many bytes.
    Written(usize),
    /// The operation returned an error; a partially delivered frame is possible.
    Failed {
        /// Portable error category.
        kind: io::ErrorKind,
        /// Native Windows error code, if available.
        raw_os_error: Option<i32>,
        /// Diagnostic supplied by the operating system.
        message: String,
    },
}

impl StdoutCompletion {
    fn from_result(result: &io::Result<usize>) -> Self {
        match result {
            Ok(count) => Self::Written(*count),
            Err(error) => Self::Failed {
                kind: error.kind(),
                raw_os_error: error.raw_os_error(),
                message: error.to_string(),
            },
        }
    }
}

enum ReadEvent {
    Data(Vec<u8>),
    Error(io::Error),
}

/// Input stream backed by one dedicated, cancellable synchronous read thread.
pub struct CancelableStdin {
    receiver: mpsc::Receiver<ReadEvent>,
    pending: Vec<u8>,
    offset: usize,
    stopping: Arc<AtomicBool>,
    drain_on_stop: bool,
}

impl AsyncRead for CancelableStdin {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        target: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if target.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        if !self.drain_on_stop && self.stopping.load(Ordering::SeqCst) {
            return Poll::Ready(Err(interrupted()));
        }
        loop {
            if self.offset < self.pending.len() {
                let end = (self.offset + target.remaining()).min(self.pending.len());
                target.put_slice(&self.pending[self.offset..end]);
                self.offset = end;
                return Poll::Ready(Ok(()));
            }
            match self.receiver.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Ready(Some(ReadEvent::Error(error))) => return Poll::Ready(Err(error)),
                Poll::Ready(Some(ReadEvent::Data(data))) => {
                    self.pending = data;
                    self.offset = 0;
                }
            }
        }
    }
}

struct WriteRequest {
    bytes: Arc<[u8]>,
    complete: oneshot::Sender<io::Result<usize>>,
}

struct InFlightWrite {
    bytes: Arc<[u8]>,
    complete: oneshot::Receiver<io::Result<usize>>,
}

/// Output stream backed by one dedicated synchronous write thread.
pub struct CancelableStdout {
    sender: Option<std_mpsc::Sender<WriteRequest>>,
    inflight: Option<InFlightWrite>,
    stopping: Arc<AtomicBool>,
    last_stdout: Arc<Mutex<Option<StdoutCompletion>>>,
    poisoned: bool,
}

impl AsyncWrite for CancelableStdout {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }
        // A submitted syscall must still report its actual result after stop;
        // cancellation of the thread is only a request, not a receipt.
        if self.poisoned {
            return Poll::Ready(Err(interrupted()));
        }
        if self.inflight.is_none() && self.stopping.load(Ordering::SeqCst) {
            return Poll::Ready(Err(write_not_started()));
        }
        if let Some(pending) = self.inflight.as_ref() {
            if pending.bytes.as_ref() != bytes {
                self.poisoned = true;
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "a pending stdout write was abandoned; close the transport",
                )));
            }
        } else {
            let bytes: Arc<[u8]> = Arc::from(bytes);
            let (complete, receiver) = oneshot::channel();
            let request = WriteRequest {
                bytes: bytes.clone(),
                complete,
            };
            let Some(sender) = self.sender.as_ref() else {
                return Poll::Ready(Err(write_not_started()));
            };
            *self
                .last_stdout
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(StdoutCompletion::Pending);
            if sender.send(request).is_err() {
                *self
                    .last_stdout
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(StdoutCompletion::NotStarted);
                return Poll::Ready(Err(write_not_started()));
            }
            self.inflight = Some(InFlightWrite {
                bytes,
                complete: receiver,
            });
        }
        let pending = self.inflight.as_mut().expect("write is in flight");
        match Pin::new(&mut pending.complete).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.inflight = None;
                let result = result.unwrap_or_else(|_| {
                    Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "stdout worker exited before reporting write completion",
                    ))
                });
                Poll::Ready(match result {
                    // AsyncWriteExt::write_all retries Interrupted. The OS
                    // may already have emitted part of this frame, so a retry
                    // must fail the connection instead of duplicating bytes.
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                        Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            format!("stdout write was interrupted: {error}"),
                        ))
                    }
                    result => result,
                })
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.inflight.is_some() || self.poisoned {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stdout has an unfinished write; close the transport",
            )));
        }
        if self.stopping.load(Ordering::SeqCst) {
            return Poll::Ready(Err(interrupted()));
        }
        // File::write is unbuffered: successful writes have already completed
        // the synchronous OS call. FlushFileBuffers would wait for a pipe's
        // reader and would not provide a bounded or cancellable flush here.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.sender.take();
        if self.inflight.is_some() || self.poisoned {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stdout has an unfinished write; use verified stdio shutdown",
            )));
        }
        Poll::Ready(Ok(()))
    }
}

fn interrupted() -> io::Error {
    // Tokio's write_all retries Interrupted; use a terminal error so a
    // cancelled partial frame can never be retried on this transport.
    io::Error::new(io::ErrorKind::BrokenPipe, "stdio transport is stopping")
}

/// A particular stdout write was rejected before entering its OS syscall.
#[derive(Debug)]
pub struct StdoutWriteNotStarted;

impl std::fmt::Display for StdoutWriteNotStarted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("stdout write was not started")
    }
}

impl std::error::Error for StdoutWriteNotStarted {}

fn write_not_started() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, StdoutWriteNotStarted)
}

/// True only for the specific write operation rejected before its OS syscall.
pub fn stdout_write_was_not_started(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|cause| cause.is::<StdoutWriteNotStarted>())
}

struct Worker {
    name: &'static str,
    thread: Option<JoinHandle<()>>,
    phase: Arc<AtomicU8>,
}

// Detached child output pipes can outlive the async operation that read them.
// A single process-lifetime reaper keeps retrying cancellation after a future
// is dropped, including the race between the worker's stop check and ReadFile.
fn read_reaper() -> io::Result<&'static std_mpsc::Sender<Worker>> {
    static REAPER: std::sync::OnceLock<Result<std_mpsc::Sender<Worker>, String>> =
        std::sync::OnceLock::new();
    match REAPER.get_or_init(|| {
        let (sender, receiver) = std_mpsc::channel::<Worker>();
        let _reaper = thread::Builder::new()
            .name("windows-pipe-read-reaper".into())
            .spawn(move || {
                let mut pending: Vec<Worker> = Vec::new();
                loop {
                    if pending.is_empty() {
                        match receiver.recv() {
                            Ok(worker) => pending.push(worker),
                            Err(_) => return,
                        }
                    } else if let Ok(worker) = receiver.recv_timeout(RETRY_INTERVAL) {
                        pending.push(worker);
                    }
                    while let Ok(worker) = receiver.try_recv() {
                        pending.push(worker);
                    }
                    pending.retain_mut(|worker| {
                        match worker.join_if_finished() {
                            Ok(true) => return false,
                            Err(_) if worker.thread.is_none() => return false,
                            _ => {}
                        }
                        let _ = worker.cancel_io();
                        true
                    });
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(sender)
    }) {
        Ok(sender) => Ok(sender),
        Err(error) => Err(io::Error::other(format!(
            "could not start pipe read reaper: {error}"
        ))),
    }
}

impl Worker {
    fn cancel_io(&self) -> io::Result<()> {
        let Some(thread) = self.thread.as_ref() else {
            return Ok(());
        };
        // SAFETY: JoinHandle owns a live thread HANDLE until it is joined or
        // dropped. This call only requests cancellation of synchronous I/O on
        // this exact dedicated thread and does not free or borrow its buffers.
        let ok = unsafe { CancelSynchronousIo(thread.as_raw_handle().cast()) };
        if ok != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
            // It may be between the stop check and its next syscall. Keep
            // retrying until the thread itself is observed to have exited.
            Ok(())
        } else {
            Err(error)
        }
    }

    fn join_if_finished(&mut self) -> io::Result<bool> {
        let Some(thread) = self.thread.as_ref() else {
            return Ok(true);
        };
        // SAFETY: the owned JoinHandle keeps the thread HANDLE valid. A zero
        // timeout never blocks. Unlike JoinHandle::is_finished(), a signalled
        // thread HANDLE confirms that the OS thread has actually terminated.
        match unsafe { WaitForSingleObject(thread.as_raw_handle().cast(), 0) } {
            WAIT_TIMEOUT => Ok(false),
            WAIT_OBJECT_0 => {
                self.thread
                    .take()
                    .expect("signalled worker exists")
                    .join()
                    .map_err(|_| {
                        io::Error::other(format!("{} stdio worker panicked", self.name))
                    })?;
                Ok(true)
            }
            WAIT_FAILED => Err(io::Error::last_os_error()),
            result => Err(io::Error::other(format!(
                "unexpected {} stdio worker wait result: {result}",
                self.name
            ))),
        }
    }
}

/// Owner of the two I/O threads. Call `shutdown` and inspect its result before
/// completing host teardown; dropping this owner only makes a best effort.
#[must_use = "call shutdown and handle its result to verify stdio worker exit"]
pub struct StdioShutdown {
    stopping: Arc<AtomicBool>,
    input: Worker,
    output: Worker,
    last_stdout: Arc<Mutex<Option<StdoutCompletion>>>,
    failures: Vec<String>,
}

/// A cloneable request to interrupt this connection's synchronous stdout I/O.
/// The caller must still await the write result and the owner's `shutdown`.
#[derive(Clone)]
pub struct StdoutInterrupt {
    stopping: Arc<AtomicBool>,
    thread: Arc<OwnedHandle>,
    phase: Arc<AtomicU8>,
}

impl StdoutInterrupt {
    /// Request cancellation once. Repeat until the worker exits: a request can
    /// race the gap immediately before the worker enters its next syscall.
    pub fn request_stop(&self) -> io::Result<()> {
        self.stopping.store(true, Ordering::SeqCst);
        // SAFETY: `thread` owns a duplicated thread HANDLE for this worker.
        let ok = unsafe { CancelSynchronousIo(self.thread.as_raw_handle().cast()) };
        if ok != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
            Ok(())
        } else {
            Err(error)
        }
    }

    /// Whether the worker has left its I/O loop. Actual OS-thread exit is
    /// verified separately by `StdioShutdown::shutdown`.
    pub fn has_left_io_loop(&self) -> bool {
        self.phase.load(Ordering::SeqCst) == EXITED
    }
}

impl StdioShutdown {
    /// Obtain an independent cancellation capability for the stdout worker.
    pub fn stdout_interrupt(&self) -> io::Result<StdoutInterrupt> {
        let thread =
            self.output.thread.as_ref().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "stdout worker has exited")
            })?;
        Ok(StdoutInterrupt {
            stopping: self.stopping.clone(),
            thread: Arc::new(thread.as_handle().try_clone_to_owned()?),
            phase: self.output.phase.clone(),
        })
    }
    /// Return the latest write status, including an operation which completed
    /// normally after a cancellation request raced with it. `Pending` is not a
    /// completion receipt and can be observed until the worker is joined.
    pub fn last_stdout_completion(&self) -> Option<StdoutCompletion> {
        self.last_stdout
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Request cancellation of both workers, then wait for actual thread exit.
    ///
    /// An I/O driver can fail to honour cancellation. In that case the deadline
    /// produces an error rather than claiming that either thread exited. The
    /// owner retains unjoined handles, so a later call can retry the shutdown.
    pub async fn shutdown(&mut self, timeout: Duration) -> io::Result<()> {
        self.stopping.store(true, Ordering::SeqCst);
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "shutdown timeout overflow")
            })?;
        let mut cancellation_errors = [None, None];
        let mut wait_errors = [None, None];
        loop {
            for (index, worker) in [&mut self.input, &mut self.output].into_iter().enumerate() {
                match worker.join_if_finished() {
                    Ok(_) => wait_errors[index] = None,
                    Err(error) if worker.thread.is_none() => {
                        self.failures.push(error.to_string());
                    }
                    Err(error) => {
                        wait_errors[index] = Some(format!("{}: {error}", worker.name));
                    }
                }
                if worker.thread.is_some() {
                    if let Err(error) = worker.cancel_io() {
                        cancellation_errors[index] = Some(format!("{}: {error}", worker.name));
                    }
                }
            }
            if self.input.thread.is_none() && self.output.thread.is_none() {
                let mut status = self
                    .last_stdout
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if *status == Some(StdoutCompletion::Pending) {
                    *status = Some(StdoutCompletion::NotStarted);
                }
                return if self.failures.is_empty() {
                    Ok(())
                } else {
                    Err(io::Error::other(self.failures.join("; ")))
                };
            }
            if tokio::time::Instant::now() >= deadline {
                let pending = [&self.input, &self.output]
                    .into_iter()
                    .filter(|worker| worker.thread.is_some())
                    .map(|worker| {
                        format!(
                            "{} (phase {})",
                            worker.name,
                            worker.phase.load(Ordering::SeqCst)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let cancellation_errors = cancellation_errors
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>();
                let detail = if cancellation_errors.is_empty() {
                    String::new()
                } else {
                    format!("; cancellation errors: {}", cancellation_errors.join("; "))
                };
                let wait_errors = wait_errors.into_iter().flatten().collect::<Vec<_>>();
                let wait_detail = if wait_errors.is_empty() {
                    String::new()
                } else {
                    format!("; wait errors: {}", wait_errors.join("; "))
                };
                let failures = if self.failures.is_empty() {
                    String::new()
                } else {
                    format!("; worker failures: {}", self.failures.join("; "))
                };
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("stdio workers did not exit: {pending}{detail}{wait_detail}{failures}"),
                ));
            }
            tokio::time::sleep_until((tokio::time::Instant::now() + RETRY_INTERVAL).min(deadline))
                .await;
        }
    }
}

impl Drop for StdioShutdown {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        let _ = self.input.cancel_io();
        let _ = self.output.cancel_io();
    }
}

/// Open independent owned handles to the current process stdin and stdout.
///
/// The caller must exclusively own these streams, with no prior buffered stdin
/// reads or outstanding buffered stdout writes, for the lifetime of the MCP
/// connection. In particular, after cancelling an in-flight stdout write, stop
/// the whole connection and call the returned owner's verified `shutdown`.
pub fn open() -> io::Result<(CancelableStdin, CancelableStdout, StdioShutdown)> {
    let input = std::io::stdin().as_handle().try_clone_to_owned()?;
    let output = std::io::stdout().as_handle().try_clone_to_owned()?;
    from_owned_handles(input, output)
}

#[cfg(test)]
#[derive(Default)]
struct ReadMetrics {
    bytes: std::sync::atomic::AtomicUsize,
    queue_full: AtomicBool,
}

#[cfg(test)]
struct TestReadGate {
    entered: std_mpsc::Sender<()>,
    release: std_mpsc::Receiver<()>,
    error: i32,
}

fn start_reader(
    input: OwnedHandle,
    stopping: Arc<AtomicBool>,
    name: &'static str,
    drain_on_stop: bool,
    snapshot: bool,
    #[cfg(test)] metrics: Option<Arc<ReadMetrics>>,
    #[cfg(test)] mut test_gate: Option<TestReadGate>,
) -> io::Result<(CancelableStdin, Worker)> {
    let phase = Arc::new(AtomicU8::new(BETWEEN_IO));
    let (read_tx, read_rx) = mpsc::channel(READ_QUEUE);
    let reader = {
        let stopping = stopping.clone();
        let phase = phase.clone();
        thread::Builder::new().name(name.into()).spawn(move || {
            let mut file = std::fs::File::from(input);
            let mut bytes = vec![0; READ_SIZE];
            // A snapshot runs only after the original reader is joined and
            // before the handoff reader starts. No other reader can remove
            // these bytes. The fixed quota makes concurrent descendant writes
            // unable to keep foreground completion open forever.
            let mut remaining = if snapshot {
                phase.store(ENTERING_IO, Ordering::SeqCst);
                let available = pipe_bytes_available(&file);
                phase.store(BETWEEN_IO, Ordering::SeqCst);
                match available {
                    Ok(count) => Some(count as usize),
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Some(0),
                    Err(error) => {
                        let _ = read_tx.blocking_send(ReadEvent::Error(error));
                        phase.store(EXITED, Ordering::SeqCst);
                        return;
                    }
                }
            } else {
                None
            };
            while !stopping.load(Ordering::SeqCst) {
                if remaining == Some(0) {
                    break;
                }
                phase.store(ENTERING_IO, Ordering::SeqCst);
                let result = if stopping.load(Ordering::SeqCst) {
                    None
                } else {
                    let length = remaining.unwrap_or(READ_SIZE).min(READ_SIZE);
                    #[cfg(test)]
                    let result = if let Some(gate) = test_gate.take() {
                        let _ = gate.entered.send(());
                        let _ = gate.release.recv();
                        Err(io::Error::from_raw_os_error(gate.error))
                    } else {
                        file.read(&mut bytes[..length])
                    };
                    #[cfg(not(test))]
                    let result = file.read(&mut bytes[..length]);
                    Some(result)
                };
                phase.store(BETWEEN_IO, Ordering::SeqCst);
                if stopping.load(Ordering::SeqCst) && !drain_on_stop {
                    break;
                }
                let event = match result {
                    None | Some(Ok(0)) => break,
                    Some(Ok(count)) => {
                        if let Some(remaining) = &mut remaining {
                            *remaining -= count;
                        }
                        #[cfg(test)]
                        if let Some(metrics) = &metrics {
                            metrics.bytes.fetch_add(count, Ordering::SeqCst);
                        }
                        ReadEvent::Data(bytes[..count].to_vec())
                    }
                    Some(Err(error))
                        if stopping.load(Ordering::SeqCst)
                            && error.raw_os_error() == Some(ERROR_OPERATION_ABORTED as i32) =>
                    {
                        break;
                    }
                    Some(Err(error)) if error.kind() == io::ErrorKind::BrokenPipe => break,
                    Some(Err(error)) => ReadEvent::Error(error),
                };
                let terminal = matches!(event, ReadEvent::Error(_));
                let mut pending = event;
                loop {
                    if stopping.load(Ordering::SeqCst) && !drain_on_stop {
                        phase.store(EXITED, Ordering::SeqCst);
                        return;
                    }
                    match read_tx.try_send(pending) {
                        Ok(()) => break,
                        Err(mpsc::error::TrySendError::Full(event)) => {
                            #[cfg(test)]
                            if let Some(metrics) = &metrics {
                                metrics.queue_full.store(true, Ordering::SeqCst);
                            }
                            pending = event;
                            thread::sleep(RETRY_INTERVAL);
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            phase.store(EXITED, Ordering::SeqCst);
                            return;
                        }
                    }
                }
                if terminal || stopping.load(Ordering::SeqCst) {
                    break;
                }
            }
            phase.store(EXITED, Ordering::SeqCst);
        })?
    };
    Ok((
        CancelableStdin {
            receiver: read_rx,
            pending: Vec::new(),
            offset: 0,
            stopping,
            drain_on_stop,
        },
        Worker {
            name,
            thread: Some(reader),
            phase,
        },
    ))
}

/// A synchronous anonymous pipe read on its own cancellable OS thread.
///
/// Unlike Tokio's Windows child stdio, this worker is not part of the runtime's
/// blocking pool. A detached descendant can therefore retain the write end
/// without keeping the supervisor's runtime alive indefinitely.
pub(crate) struct CancelablePipeReader {
    input: CancelableStdin,
    worker: Option<Worker>,
    #[cfg(test)]
    metrics: Arc<ReadMetrics>,
}

impl CancelablePipeReader {
    pub(crate) fn new(input: std::io::PipeReader, name: &'static str) -> io::Result<Self> {
        Self::with_snapshot(input, name, false)
    }

    pub(crate) fn snapshot(input: std::io::PipeReader, name: &'static str) -> io::Result<Self> {
        Self::with_snapshot(input, name, true)
    }

    fn with_snapshot(
        input: std::io::PipeReader,
        name: &'static str,
        snapshot: bool,
    ) -> io::Result<Self> {
        // Establish the fallback before starting a worker, so an abandoned
        // read always has somewhere to transfer its cancellation capability.
        read_reaper()?;
        let stopping = Arc::new(AtomicBool::new(false));
        #[cfg(test)]
        let metrics = Arc::new(ReadMetrics::default());
        let (input, worker) = start_reader(
            input.into(),
            stopping,
            name,
            true,
            snapshot,
            #[cfg(test)]
            Some(metrics.clone()),
            #[cfg(test)]
            None,
        )?;
        Ok(Self {
            input,
            worker: Some(worker),
            #[cfg(test)]
            metrics,
        })
    }

    pub(crate) fn request_stop(&self) {
        self.input.stopping.store(true, Ordering::SeqCst);
        if let Some(worker) = &self.worker {
            let _ = worker.cancel_io();
        }
    }

    pub(crate) async fn shutdown(&mut self, timeout: Duration) -> io::Result<()> {
        self.request_stop();
        let deadline = tokio::time::Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "shutdown timeout overflow")
            })?;
        let mut last_error = None;
        while let Some(worker) = &mut self.worker {
            match worker.join_if_finished() {
                Ok(true) => {
                    self.worker = None;
                    return Ok(());
                }
                Err(error) if worker.thread.is_none() => {
                    self.worker = None;
                    return Err(error);
                }
                Err(error) => last_error = Some(error.to_string()),
                Ok(false) => {}
            }
            if let Err(error) = worker.cancel_io() {
                last_error = Some(error.to_string());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "{} pipe reader did not exit (phase {}, last error: {})",
                        worker.name,
                        worker.phase.load(Ordering::SeqCst),
                        last_error.as_deref().unwrap_or("none")
                    ),
                ));
            }
            tokio::time::sleep_until((tokio::time::Instant::now() + RETRY_INTERVAL).min(deadline))
                .await;
        }
        Ok(())
    }
}

impl AsyncRead for CancelablePipeReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        target: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.input).poll_read(cx, target)
    }
}

impl Drop for CancelablePipeReader {
    fn drop(&mut self) {
        self.request_stop();
        if let Some(worker) = self.worker.take() {
            // This reaper was started before the reader. It owns the original
            // thread handle and retries until the actual OS thread exits.
            match read_reaper().and_then(|reaper| {
                reaper
                    .send(worker)
                    .map_err(|_| io::Error::other("pipe read reaper exited"))
            }) {
                Ok(()) => {}
                Err(error) => tracing::error!(%error, "could not retain pipe read cancellation"),
            }
        }
    }
}

fn from_owned_handles(
    input: OwnedHandle,
    output: OwnedHandle,
) -> io::Result<(CancelableStdin, CancelableStdout, StdioShutdown)> {
    #[cfg(test)]
    {
        from_owned_handles_impl(input, output, None)
    }
    #[cfg(not(test))]
    {
        from_owned_handles_impl(input, output)
    }
}

#[cfg(test)]
struct TestWriteGate {
    entered: std_mpsc::Sender<()>,
    release: std_mpsc::Receiver<()>,
}

fn from_owned_handles_impl(
    input: OwnedHandle,
    output: OwnedHandle,
    #[cfg(test)] test_gate: Option<TestWriteGate>,
) -> io::Result<(CancelableStdin, CancelableStdout, StdioShutdown)> {
    let stopping = Arc::new(AtomicBool::new(false));
    let write_phase = Arc::new(AtomicU8::new(BETWEEN_IO));
    let last_stdout = Arc::new(Mutex::new(None));
    let (write_tx, write_rx) = std_mpsc::channel::<WriteRequest>();

    // Start the writer first. If the reader cannot be started, the writer has
    // no work; shutdown still verifies its exit with a bounded OS wait.
    let writer = {
        let stopping = stopping.clone();
        let phase = write_phase.clone();
        let last_stdout = last_stdout.clone();
        thread::Builder::new()
            .name("mcp-stdio-writer".into())
            .spawn(move || {
                let mut file = std::fs::File::from(output);
                while !stopping.load(Ordering::SeqCst) {
                    let request = match write_rx.recv_timeout(RETRY_INTERVAL) {
                        Ok(request) => request,
                        Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    phase.store(ENTERING_IO, Ordering::SeqCst);
                    let (result, completion) = if stopping.load(Ordering::SeqCst) {
                        (Err(write_not_started()), StdoutCompletion::NotStarted)
                    } else {
                        let result = file.write(&request.bytes);
                        let completion = StdoutCompletion::from_result(&result);
                        (result, completion)
                    };
                    phase.store(BETWEEN_IO, Ordering::SeqCst);
                    *last_stdout
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(completion);
                    #[cfg(test)]
                    if request.bytes.as_ref() == b"\n" && result.is_ok() {
                        if let Some(gate) = &test_gate {
                            let _ = gate.entered.send(());
                            let _ = gate.release.recv();
                        }
                    }
                    if request.complete.send(result).is_err() {
                        break;
                    }
                }
                // A stop may arrive before the worker receives its one queued
                // request. Report that it did not enter WriteFile instead of
                // leaving the result of an older write as the last completion.
                while let Ok(request) = write_rx.try_recv() {
                    *last_stdout
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(StdoutCompletion::NotStarted);
                    let _ = request.complete.send(Err(write_not_started()));
                }
                phase.store(EXITED, Ordering::SeqCst);
            })?
    };
    let (input, reader) = match start_reader(
        input,
        stopping.clone(),
        "mcp-stdio-reader",
        false,
        false,
        #[cfg(test)]
        None,
        #[cfg(test)]
        None,
    ) {
        Ok(reader) => reader,
        Err(error) => {
            stopping.store(true, Ordering::SeqCst);
            drop(write_tx);
            // SAFETY: the JoinHandle keeps the thread HANDLE valid. This is a
            // bounded wait; the writer has no request and cannot enter I/O.
            let cleanup = match unsafe { WaitForSingleObject(writer.as_raw_handle().cast(), 1000) }
            {
                WAIT_OBJECT_0 => writer
                    .join()
                    .map_err(|_| "stdout worker panicked during cleanup".to_owned()),
                WAIT_TIMEOUT => Err("stdout worker did not exit within one second".to_owned()),
                WAIT_FAILED => Err(format!(
                    "could not wait for stdout worker: {}",
                    io::Error::last_os_error()
                )),
                result => Err(format!("unexpected stdout worker wait result: {result}")),
            };
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(io::Error::new(
                    error.kind(),
                    format!("could not start stdin worker: {error}; {cleanup_error}"),
                )),
            };
        }
    };

    Ok((
        input,
        CancelableStdout {
            sender: Some(write_tx),
            inflight: None,
            stopping: stopping.clone(),
            last_stdout: last_stdout.clone(),
            poisoned: false,
        },
        StdioShutdown {
            stopping,
            input: reader,
            output: Worker {
                name: "stdout",
                thread: Some(writer),
                phase: write_phase,
            },
            last_stdout,
            failures: Vec::new(),
        },
    ))
}

/// Return bytes currently buffered on a Windows pipe's read end.
///
/// This is intended for a controlled test fixture. Microsoft documents that a
/// synchronous `PeekNamedPipe` can block in a multithreaded process, so do not
/// call it on an async executor thread or treat it as production readiness.
#[doc(hidden)]
pub fn pipe_bytes_available(handle: &impl AsHandle) -> io::Result<u32> {
    let mut available = 0;
    // SAFETY: the borrowed handle remains valid during the call, and the only
    // non-null pointer refers to the initialized, writable `available` value.
    let ok = unsafe {
        PeekNamedPipe(
            handle.as_handle().as_raw_handle().cast(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(available)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn owned(handle: &impl AsHandle) -> io::Result<OwnedHandle> {
        handle.as_handle().try_clone_to_owned()
    }

    #[tokio::test]
    async fn child_pipe_reader_stops_while_writer_remains_open() -> io::Result<()> {
        let (read, mut write) = std::io::pipe()?;
        let mut reader = CancelablePipeReader::new(read, "test child output")?;
        write.write_all(b"before stop")?;
        let mut prefix = [0; 11];
        tokio::time::timeout(Duration::from_secs(5), reader.read_exact(&mut prefix)).await??;
        assert_eq!(&prefix, b"before stop");
        let mut next = [0; 1];
        assert!(
            tokio::time::timeout(Duration::from_millis(50), reader.read(&mut next))
                .await
                .is_err()
        );
        reader.shutdown(Duration::from_secs(5)).await?;
        assert_eq!(
            write.write(b"after stop").unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        Ok(())
    }

    #[tokio::test]
    async fn child_pipe_reader_drains_past_queue_capacity() -> io::Result<()> {
        let (read, mut write) = std::io::pipe()?;
        let mut reader = CancelablePipeReader::new(read, "test child output")?;
        let bytes = (0..READ_SIZE * (READ_QUEUE + 2))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let expected = bytes.clone();
        let writer = tokio::task::spawn_blocking(move || write.write_all(&bytes));
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_to_end(&mut received)).await??;
        writer.await.map_err(io::Error::other)??;
        assert_eq!(received, expected);
        reader.shutdown(Duration::from_secs(5)).await?;
        Ok(())
    }

    #[tokio::test]
    async fn child_pipe_snapshot_reads_buffered_bytes_without_waiting_for_writer() -> io::Result<()>
    {
        let (mut read, mut write) = std::io::pipe()?;
        write.write_all(b"foreground tail")?;
        let mut snapshot = CancelablePipeReader::snapshot(read.try_clone()?, "test snapshot")?;
        let mut tail = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), snapshot.read_to_end(&mut tail)).await??;
        snapshot.shutdown(Duration::from_secs(5)).await?;
        assert_eq!(&tail, b"foreground tail");
        write.write_all(b"later")?;
        let mut later = [0u8; 5];
        read.read_exact(&mut later)?;
        assert_eq!(&later, b"later");
        Ok(())
    }

    #[tokio::test]
    async fn stopping_full_child_pipe_queue_delivers_every_completed_read() -> io::Result<()> {
        let (read, mut write) = std::io::pipe()?;
        let mut reader = CancelablePipeReader::new(read, "test full child output")?;
        let thread = reader
            .worker
            .as_ref()
            .unwrap()
            .thread
            .as_ref()
            .unwrap()
            .as_handle()
            .try_clone_to_owned()?;
        let metrics = reader.metrics.clone();
        let writer = tokio::task::spawn_blocking(move || write.write_all(&vec![0x51; 1024 * 1024]));
        tokio::time::timeout(Duration::from_secs(5), async {
            while !metrics.queue_full.load(Ordering::SeqCst) {
                tokio::time::sleep(RETRY_INTERVAL).await;
            }
        })
        .await?;
        reader.request_stop();
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_to_end(&mut received)).await??;
        reader.shutdown(Duration::from_secs(5)).await?;
        // SAFETY: the duplicate retains this exact worker's thread object.
        assert_eq!(
            unsafe { WaitForSingleObject(thread.as_raw_handle().cast(), 0) },
            WAIT_OBJECT_0
        );
        let _ = writer.await.map_err(io::Error::other)?;
        assert_eq!(received.len(), metrics.bytes.load(Ordering::SeqCst));
        assert!(received.len() > READ_QUEUE);
        assert!(received.iter().all(|byte| *byte == 0x51));
        Ok(())
    }

    #[tokio::test]
    async fn child_pipe_reader_reports_real_error_racing_stop() -> io::Result<()> {
        let (input, _write) = std::io::pipe()?;
        let stopping = Arc::new(AtomicBool::new(false));
        let metrics = Arc::new(ReadMetrics::default());
        let (entered, entered_rx) = std_mpsc::channel();
        let (release, release_rx) = std_mpsc::channel();
        let (input, worker) = start_reader(
            input.into(),
            stopping,
            "test stopped read error",
            true,
            false,
            Some(metrics.clone()),
            Some(TestReadGate {
                entered,
                release: release_rx,
                error: 5, // ERROR_ACCESS_DENIED, not ERROR_OPERATION_ABORTED.
            }),
        )?;
        let mut reader = CancelablePipeReader {
            input,
            worker: Some(worker),
            metrics,
        };
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(io::Error::other)?;
        reader.request_stop();
        release.send(()).map_err(io::Error::other)?;
        let mut byte = [0u8; 1];
        let error = tokio::time::timeout(Duration::from_secs(5), reader.read(&mut byte))
            .await?
            .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(5));
        reader.shutdown(Duration::from_secs(5)).await?;
        Ok(())
    }

    #[tokio::test]
    async fn dropped_child_pipe_reader_is_joined_without_closing_writer() -> io::Result<()> {
        let (read, _write) = std::io::pipe()?;
        let reader = CancelablePipeReader::new(read, "test dropped child output")?;
        let thread = reader
            .worker
            .as_ref()
            .unwrap()
            .thread
            .as_ref()
            .unwrap()
            .as_handle()
            .try_clone_to_owned()?;
        drop(reader);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // SAFETY: `thread` retains this worker's OS thread handle.
                match unsafe { WaitForSingleObject(thread.as_raw_handle().cast(), 0) } {
                    WAIT_OBJECT_0 => break,
                    WAIT_TIMEOUT => tokio::time::sleep(RETRY_INTERVAL).await,
                    _ => panic!("could not wait for dropped pipe reader"),
                }
            }
        })
        .await?;
        Ok(())
    }

    #[tokio::test]
    async fn anonymous_pipes_preserve_bytes_and_join_both_workers() -> io::Result<()> {
        let (input_read, mut input_write) = std::io::pipe()?;
        let (mut output_read, output_write) = std::io::pipe()?;
        let (mut input, mut output, mut shutdown) =
            from_owned_handles(owned(&input_read)?, owned(&output_write)?)?;
        drop(input_read);
        drop(output_write);

        input_write.write_all(b"partial")?;
        input_write.write_all(b" line\nnext\n")?;
        drop(input_write);
        let mut received = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), input.read_to_end(&mut received)).await??;
        assert_eq!(received, b"partial line\nnext\n");

        output.write_all(b"one\ntwo\n").await?;
        output.flush().await?;
        assert_eq!(
            shutdown.last_stdout_completion(),
            Some(StdoutCompletion::Written(8))
        );
        shutdown.shutdown(Duration::from_secs(5)).await?;
        let mut emitted = Vec::new();
        output_read.read_to_end(&mut emitted)?;
        assert_eq!(emitted, b"one\ntwo\n");
        assert!(shutdown.input.thread.is_none());
        assert!(shutdown.output.thread.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn completed_pipe_write_is_reported_after_interruption_even_when_notification_is_delayed(
    ) -> io::Result<()> {
        let (input_read, _input_write) = std::io::pipe()?;
        let (mut output_read, output_write) = std::io::pipe()?;
        let (entered, observed) = std_mpsc::channel();
        let (release, resume) = std_mpsc::channel();
        let (_input, mut output, mut shutdown) = from_owned_handles_impl(
            owned(&input_read)?,
            owned(&output_write)?,
            Some(TestWriteGate {
                entered,
                release: resume,
            }),
        )?;
        drop(input_read);
        drop(output_write);
        output.write_all(b"{}").await?;
        let write = tokio::spawn(async move { output.write_all(b"\n").await });
        tokio::task::spawn_blocking(move || observed.recv_timeout(Duration::from_secs(5)))
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?;
        let visible = tokio::task::spawn_blocking(move || -> io::Result<_> {
            let mut visible = [0u8; 3];
            output_read.read_exact(&mut visible)?;
            Ok((output_read, visible))
        })
        .await
        .map_err(io::Error::other)??;
        assert_eq!(&visible.1, b"{}\n");
        let interrupt = shutdown.stdout_interrupt()?;
        interrupt.request_stop()?;
        assert!(!write.is_finished(), "the worker completion is still gated");
        release.send(()).map_err(io::Error::other)?;
        tokio::time::timeout(Duration::from_secs(5), write).await???;
        shutdown.shutdown(Duration::from_secs(5)).await?;
        Ok(())
    }

    #[tokio::test]
    async fn not_started_receipt_is_specific_to_the_rejected_write() -> io::Result<()> {
        let (input_read, _input_write) = std::io::pipe()?;
        let (_output_read, output_write) = std::io::pipe()?;
        let (_input, mut output, mut shutdown) =
            from_owned_handles(owned(&input_read)?, owned(&output_write)?)?;
        shutdown.stdout_interrupt()?.request_stop()?;
        let error = output.write_all(b"\n").await.unwrap_err();
        assert!(stdout_write_was_not_started(&error));
        shutdown.shutdown(Duration::from_secs(5)).await?;

        let (input_read, _input_write) = std::io::pipe()?;
        let (output_read, output_write) = std::io::pipe()?;
        let (_input, mut output, mut shutdown) =
            from_owned_handles(owned(&input_read)?, owned(&output_write)?)?;
        drop(output_read);
        let error = output.write_all(b"\n").await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(!stdout_write_was_not_started(&error));
        shutdown.shutdown(Duration::from_secs(5)).await?;
        Ok(())
    }

    #[tokio::test]
    async fn full_stdout_and_empty_stdin_are_cancelled_and_joined() -> io::Result<()> {
        let (input_read, _input_write) = std::io::pipe()?;
        let (mut output_read, output_write) = std::io::pipe()?;
        let (_input, mut output, mut shutdown) =
            from_owned_handles(owned(&input_read)?, owned(&output_write)?)?;
        drop(input_read);
        drop(output_write);
        let write =
            tokio::spawn(async move { output.write_all(&vec![b'x'; 8 * 1024 * 1024]).await });

        // Never peek on an async executor thread: Microsoft documents that a
        // synchronous PeekNamedPipe can itself block in a multithreaded process.
        let probe = tokio::task::spawn_blocking(move || -> io::Result<_> {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let available = pipe_bytes_available(&output_read)?;
                if available >= 4096 {
                    // Peeked bytes may belong to the outstanding WriteFile and
                    // disappear when it is cancelled. Consume a real prefix
                    // while the write is still pending, so the test proves a
                    // partial frame was delivered without assuming that the
                    // remaining, unread bytes survive cancellation.
                    let mut emitted = vec![0; 4096];
                    output_read.read_exact(&mut emitted)?;
                    return Ok((output_read, emitted));
                }
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "stdout did not reach backpressure",
                    ));
                }
                thread::sleep(RETRY_INTERVAL);
            }
        });
        let (mut output_read, mut emitted) =
            tokio::time::timeout(Duration::from_secs(6), probe).await???;
        assert_eq!(emitted, vec![b'x'; 4096]);
        assert!(!write.is_finished(), "large write must still be in flight");
        assert_eq!(
            shutdown.output.phase.load(Ordering::SeqCst),
            ENTERING_IO,
            "stdout worker must still be entering or blocked in I/O"
        );
        assert_eq!(
            shutdown.last_stdout_completion(),
            Some(StdoutCompletion::Pending)
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while shutdown.input.phase.load(Ordering::SeqCst) != ENTERING_IO {
                tokio::time::sleep(RETRY_INTERVAL).await;
            }
        })
        .await?;
        shutdown.shutdown(Duration::from_secs(5)).await?;
        let result = tokio::time::timeout(Duration::from_secs(5), write).await??;
        assert!(
            result.is_err(),
            "cancelled output must not be accepted as a complete frame"
        );
        assert!(matches!(
            shutdown.last_stdout_completion(),
            Some(
                StdoutCompletion::NotStarted
                    | StdoutCompletion::Written(_)
                    | StdoutCompletion::Failed { .. }
            )
        ));
        assert!(shutdown.input.thread.is_none());
        assert!(shutdown.output.thread.is_none());
        output_read.read_to_end(&mut emitted)?;
        assert!(!emitted.is_empty());
        assert!(emitted.iter().all(|byte| *byte == b'x'));
        assert!(emitted.len() < 8 * 1024 * 1024);
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_retries_when_the_first_request_precedes_readfile() -> io::Result<()> {
        let (input, _keep_open) = std::io::pipe()?;
        let mut file = std::fs::File::from(owned(&input)?);
        let (entered_tx, entered_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let phase = Arc::new(AtomicU8::new(BETWEEN_IO));
        let worker_phase = phase.clone();
        let reader = thread::spawn(move || {
            worker_phase.store(ENTERING_IO, Ordering::SeqCst);
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
            // This intentionally starts the syscall after the first cancel
            // attempt, reproducing the check-to-ReadFile race.
            let mut byte = [0];
            let _ = file.read(&mut byte);
            worker_phase.store(EXITED, Ordering::SeqCst);
        });
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .map_err(io::Error::other)?;
        let output = thread::spawn(|| {});
        let mut shutdown = StdioShutdown {
            stopping: Arc::new(AtomicBool::new(false)),
            input: Worker {
                name: "stdin",
                thread: Some(reader),
                phase,
            },
            output: Worker {
                name: "stdout",
                thread: Some(output),
                phase: Arc::new(AtomicU8::new(EXITED)),
            },
            last_stdout: Arc::new(Mutex::new(None)),
            failures: Vec::new(),
        };
        let mut stopping = Box::pin(shutdown.shutdown(Duration::from_secs(5)));
        std::future::poll_fn(|cx| {
            assert!(stopping.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        release_tx.send(()).map_err(io::Error::other)?;
        stopping.await?;
        Ok(())
    }

    #[tokio::test]
    async fn unverified_timeout_is_an_error_and_preserves_the_worker_for_retry() -> io::Result<()> {
        let (release, wait) = std_mpsc::channel();
        let input = thread::spawn(move || {
            let _ = wait.recv();
        });
        let output = thread::spawn(|| {});
        let mut shutdown = StdioShutdown {
            stopping: Arc::new(AtomicBool::new(false)),
            input: Worker {
                name: "stdin",
                thread: Some(input),
                phase: Arc::new(AtomicU8::new(BETWEEN_IO)),
            },
            output: Worker {
                name: "stdout",
                thread: Some(output),
                phase: Arc::new(AtomicU8::new(EXITED)),
            },
            last_stdout: Arc::new(Mutex::new(None)),
            failures: Vec::new(),
        };
        let error = shutdown.shutdown(Duration::ZERO).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(shutdown.input.thread.is_some());
        release.send(()).map_err(io::Error::other)?;
        shutdown.shutdown(Duration::from_secs(5)).await?;
        assert!(shutdown.input.thread.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn changed_buffer_after_pending_write_fails_closed() -> io::Result<()> {
        let (input_read, _input_write) = std::io::pipe()?;
        let (_output_read, output_write) = std::io::pipe()?;
        let (_input, mut output, mut shutdown) =
            from_owned_handles(owned(&input_read)?, owned(&output_write)?)?;
        let large = vec![b'a'; 8 * 1024 * 1024];
        std::future::poll_fn(|cx| {
            assert!(Pin::new(&mut output).poll_write(cx, &large).is_pending());
            Poll::Ready(())
        })
        .await;
        let error = std::future::poll_fn(|cx| Pin::new(&mut output).poll_write(cx, b"new frame"))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        shutdown.shutdown(Duration::from_secs(5)).await?;
        Ok(())
    }
}
