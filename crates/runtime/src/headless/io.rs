//! Host-owned byte streams. No process stdio or terminal state is read here.
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

pub type Input = Pin<Box<dyn AsyncRead + Send>>;
type Writer = Pin<Box<dyn AsyncWrite + Send>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputError {
    pub kind: io::ErrorKind,
    pub message: String,
}
impl OutputError {
    pub fn into_io_error(self) -> io::Error {
        io::Error::new(self.kind, self.message)
    }
}
struct OutputInner {
    writer: Mutex<Writer>,
    error: StdMutex<Option<OutputError>>,
    delivery_stop: tokio_util::sync::CancellationToken,
    terminal: bool,
}
/// An injected writer shared by all output producers. The first delivery
/// failure remains observable even when an OutputStream callback returns ().
#[derive(Clone)]
pub struct Output {
    inner: Arc<OutputInner>,
}
impl Output {
    pub fn new(writer: impl AsyncWrite + Send + 'static) -> Self {
        Self::with_terminal(writer, false)
    }
    pub fn with_terminal(writer: impl AsyncWrite + Send + 'static, terminal: bool) -> Self {
        Self {
            inner: Arc::new(OutputInner {
                writer: Mutex::new(Box::pin(writer)),
                error: StdMutex::new(None),
                delivery_stop: tokio_util::sync::CancellationToken::new(),
                terminal,
            }),
        }
    }
    pub fn is_terminal(&self) -> bool {
        self.inner.terminal
    }
    pub fn error(&self) -> Option<OutputError> {
        self.inner
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub(crate) fn record_error(&self, error: &io::Error) {
        let mut first = self.inner.error.lock().unwrap_or_else(|e| e.into_inner());
        first.get_or_insert_with(|| OutputError {
            kind: error.kind(),
            message: error.to_string(),
        });
        drop(first);
        self.inner.delivery_stop.cancel();
    }
    /// Stop delivery independently of input/model cancellation. This also
    /// wakes producers waiting for the writer lease or an output flush.
    pub fn abort_delivery(&self, reason: impl Into<String>) {
        self.record_error(&io::Error::new(io::ErrorKind::Interrupted, reason.into()));
    }
    pub async fn delivery_cancelled(&self) {
        self.inner.delivery_stop.cancelled().await;
    }
    pub fn delivery_error(&self) -> io::Error {
        self.error()
            .map(OutputError::into_io_error)
            .unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    "headless output delivery stopped",
                )
            })
    }
    async fn deliver<T>(&self, operation: impl Future<Output = io::Result<T>>) -> io::Result<T> {
        let result = tokio::select! {
            biased;
            _ = self.delivery_cancelled() => Err(self.delivery_error()),
            result = operation => result,
        };
        if let Err(error) = &result {
            self.record_error(error);
        }
        result
    }
    pub async fn write_all(&self, bytes: &[u8]) -> io::Result<()> {
        self.deliver(async { self.inner.writer.lock().await.write_all(bytes).await })
            .await
    }
    /// Write and flush one complete byte record under one writer lease.
    pub async fn write_record(&self, bytes: &[u8]) -> io::Result<()> {
        self.deliver(async {
            let mut writer = self.inner.writer.lock().await;
            writer.write_all(bytes).await?;
            writer.flush().await
        })
        .await
    }
    pub async fn write_line(&self, line: &str) -> io::Result<()> {
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        self.write_record(&bytes).await
    }
    pub async fn flush(&self) -> io::Result<()> {
        self.deliver(async { self.inner.writer.lock().await.flush().await })
            .await
    }
}
pub struct HeadlessIo {
    pub input: Input,
    /// Supplied by the host; print mode never waits for terminal input.
    pub input_is_terminal: bool,
    pub stdout: Output,
    pub stderr: Output,
}
impl HeadlessIo {
    pub fn new(
        input: impl AsyncRead + Send + 'static,
        stdout: impl AsyncWrite + Send + 'static,
        stderr: impl AsyncWrite + Send + 'static,
    ) -> Self {
        Self {
            input: Box::pin(input),
            input_is_terminal: false,
            stdout: Output::new(stdout),
            stderr: Output::new(stderr),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{Context, Poll};
    use tokio::sync::Notify;

    struct PendingWriter {
        polled: Arc<Notify>,
        pending_flush: bool,
    }
    impl AsyncWrite for PendingWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.pending_flush {
                Poll::Ready(Ok(bytes.len()))
            } else {
                self.polled.notify_one();
                Poll::Pending
            }
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.polled.notify_one();
            Poll::Pending
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }

    #[tokio::test]
    async fn abort_unblocks_permanent_pending_write_and_writer_lease_waiters() {
        let polled = Arc::new(Notify::new());
        let output = Output::new(PendingWriter {
            polled: polled.clone(),
            pending_flush: false,
        });
        let writer = output.clone();
        let writing = tokio::spawn(async move { writer.write_all(b"result").await });
        polled.notified().await;
        let waiter = output.clone();
        let flushing = tokio::spawn(async move { waiter.flush().await });
        let waiter = output.clone();
        let second_write = tokio::spawn(async move { waiter.write_record(b"control").await });
        tokio::task::yield_now().await;
        assert!(!flushing.is_finished());
        assert!(!second_write.is_finished());
        output.abort_delivery("host waiter dropped");
        for task in [writing, flushing, second_write] {
            let error = tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
            assert_eq!(error.to_string(), "host waiter dropped");
        }
        assert!(output.inner.writer.try_lock().is_ok());
        output.abort_delivery("later termination");
        assert_eq!(output.error().unwrap().message, "host waiter dropped");
    }

    #[tokio::test]
    async fn abort_unblocks_permanent_pending_flush() {
        let polled = Arc::new(Notify::new());
        let output = Output::new(PendingWriter {
            polled: polled.clone(),
            pending_flush: true,
        });
        let writer = output.clone();
        let task = tokio::spawn(async move { writer.write_record(b"result\n").await });
        polled.notified().await;
        output.abort_delivery("terminate");
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert!(output.inner.writer.try_lock().is_ok());
    }

    #[tokio::test]
    async fn abort_unblocks_a_full_duplex_peer_while_peer_stays_open() {
        use tokio::io::AsyncReadExt;
        let (writer, mut held_peer) = tokio::io::duplex(1);
        let output = Output::new(writer);
        let writer = output.clone();
        let task = tokio::spawn(async move { writer.write_record(&[b'x'; 64]).await });
        let mut first_byte = [0];
        held_peer.read_exact(&mut first_byte).await.unwrap();
        assert_eq!(first_byte, [b'x']);
        output.abort_delivery("terminate");
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert!(output.inner.writer.try_lock().is_ok());
    }

    #[tokio::test]
    async fn an_io_failure_survives_later_delivery_cancellation() {
        let (writer, peer) = tokio::io::duplex(1);
        drop(peer);
        let output = Output::new(writer);
        assert_eq!(
            output.write_all(b"x").await.unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        output.abort_delivery("terminate");
        assert_eq!(
            output.flush().await.unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(output.error().unwrap().kind, io::ErrorKind::BrokenPipe);
    }
}
