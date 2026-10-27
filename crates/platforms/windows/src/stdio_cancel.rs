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
use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
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
        if self.stopping.load(Ordering::SeqCst) {
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
    let read_phase = Arc::new(AtomicU8::new(BETWEEN_IO));
    let write_phase = Arc::new(AtomicU8::new(BETWEEN_IO));
    let last_stdout = Arc::new(Mutex::new(None));
    let (read_tx, read_rx) = mpsc::channel(READ_QUEUE);
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
    let reader = {
        let stopping = stopping.clone();
        let phase = read_phase.clone();
        thread::Builder::new()
            .name("mcp-stdio-reader".into())
            .spawn(move || {
                let mut file = std::fs::File::from(input);
                let mut bytes = vec![0; READ_SIZE];
                while !stopping.load(Ordering::SeqCst) {
                    phase.store(ENTERING_IO, Ordering::SeqCst);
                    let result = if stopping.load(Ordering::SeqCst) {
                        Err(interrupted())
                    } else {
                        file.read(&mut bytes)
                    };
                    phase.store(BETWEEN_IO, Ordering::SeqCst);
                    if stopping.load(Ordering::SeqCst) {
                        break;
                    }
                    let event = match result {
                        Ok(0) => break,
                        Ok(count) => ReadEvent::Data(bytes[..count].to_vec()),
                        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break,
                        Err(error) => ReadEvent::Error(error),
                    };
                    let terminal = matches!(event, ReadEvent::Error(_));
                    let mut pending = event;
                    loop {
                        if stopping.load(Ordering::SeqCst) {
                            phase.store(EXITED, Ordering::SeqCst);
                            return;
                        }
                        match read_tx.try_send(pending) {
                            Ok(()) => break,
                            Err(mpsc::error::TrySendError::Full(event)) => {
                                pending = event;
                                thread::sleep(RETRY_INTERVAL);
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                phase.store(EXITED, Ordering::SeqCst);
                                return;
                            }
                        }
                    }
                    if terminal {
                        break;
                    }
                }
                phase.store(EXITED, Ordering::SeqCst);
            })
    };
    let reader = match reader {
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
        CancelableStdin {
            receiver: read_rx,
            pending: Vec::new(),
            offset: 0,
            stopping: stopping.clone(),
        },
        CancelableStdout {
            sender: Some(write_tx),
            inflight: None,
            stopping: stopping.clone(),
            last_stdout: last_stdout.clone(),
            poisoned: false,
        },
        StdioShutdown {
            stopping,
            input: Worker {
                name: "stdin",
                thread: Some(reader),
                phase: read_phase,
            },
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
        let (output_read, output_write) = std::io::pipe()?;
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
                    return Ok(output_read);
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
        let mut output_read = tokio::time::timeout(Duration::from_secs(6), probe).await???;
        assert!(!write.is_finished(), "large write must still be in flight");
        shutdown.shutdown(Duration::from_secs(5)).await?;
        let result = tokio::time::timeout(Duration::from_secs(5), write).await??;
        assert!(
            result.is_err(),
            "cancelled output must not be accepted as a complete frame"
        );
        assert!(shutdown.last_stdout_completion().is_some());
        let mut emitted = Vec::new();
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
