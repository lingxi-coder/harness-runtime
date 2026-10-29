//! Admission and owned completion for foreground-to-background publication.

use super::BgForkError;
use futures_util::FutureExt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

#[derive(Default)]
struct State {
    closing: bool,
    active: usize,
    failures: Vec<String>,
}

/// Closing and claiming use the same lock. An accepted launch moves its claim
/// into an owned task before its first I/O await, so abandoning a caller cannot
/// make host shutdown overtake a partially published launch.
#[derive(Default)]
pub struct BackgroundLaunchLifecycle {
    state: Mutex<State>,
    changed: tokio::sync::Notify,
}

impl BackgroundLaunchLifecycle {
    /// Reject identity reads once the foreground owner closes admission.
    pub fn ensure_open(&self) -> Result<(), BgForkError> {
        if self.state.lock().unwrap_or_else(|p| p.into_inner()).closing {
            Err(BgForkError::Dispatch(
                "foreground session is shutting down".into(),
            ))
        } else {
            Ok(())
        }
    }

    /// Admit one launch atomically with respect to permanent closure.
    pub fn claim(self: &Arc<Self>) -> Result<BackgroundLaunchClaim, BgForkError> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closing {
            return Err(BgForkError::Dispatch(
                "foreground session is shutting down".into(),
            ));
        }
        state.active = state.active.checked_add(1).ok_or_else(|| {
            BgForkError::Dispatch("too many concurrent background launches".into())
        })?;
        Ok(BackgroundLaunchClaim {
            lifecycle: self.clone(),
            started: false,
            settled: false,
        })
    }

    /// Synchronously revoke admission before the host awaits any producer.
    pub fn close(&self) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).closing = true;
    }

    /// Cancellation only abandons this waiter. Claims remain in their owned
    /// jobs, and a retry observes the same permanently closed admission fence.
    pub async fn close_and_drain(&self) -> Result<(), BgForkError> {
        self.close();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                if state.active == 0 {
                    return if state.failures.is_empty() {
                        Ok(())
                    } else {
                        Err(BgForkError::Dispatch(state.failures.join("; ")))
                    };
                }
            }
            changed.await;
        }
    }
}

/// A claim can be dropped before publication starts. Once I/O is admitted,
/// `run_owned` transfers it to the actual worker rather than the awaiting UI.
pub struct BackgroundLaunchClaim {
    lifecycle: Arc<BackgroundLaunchLifecycle>,
    started: bool,
    settled: bool,
}

type CaughtLaunchFuture<T> = Pin<
    Box<dyn Future<Output = Result<Result<T, BgForkError>, Box<dyn std::any::Any + Send>>> + Send>,
>;

struct OwnedLaunch<T> {
    work: Option<CaughtLaunchFuture<T>>,
    claim: Option<BackgroundLaunchClaim>,
    reply: Option<tokio::sync::oneshot::Sender<Result<T, BgForkError>>>,
}

impl<T> Unpin for OwnedLaunch<T> {}

impl<T> Future for OwnedLaunch<T> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        this.claim.as_mut().expect("launch claim").started = true;
        let outcome = match this.work.as_mut().expect("launch work").as_mut().poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(result) => result,
        };
        // Release the operation and everything it owns before the claim.
        drop(this.work.take());
        let claim = this.claim.as_mut().expect("launch claim");
        let outcome = match outcome {
            Ok(result) => result,
            Err(_) => {
                let message = "background launch owner panicked".to_string();
                claim
                    .lifecycle
                    .state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .failures
                    .push(message.clone());
                Err(BgForkError::Dispatch(message))
            }
        };
        if let Err(BgForkError::OwnershipUncertain(message)) = &outcome {
            claim
                .lifecycle
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .failures
                .push(message.clone());
        }
        claim.settled = true;
        drop(this.claim.take());
        if let Some(reply) = this.reply.take() {
            let _ = reply.send(outcome);
        }
        Poll::Ready(())
    }
}

impl<T> Drop for OwnedLaunch<T> {
    fn drop(&mut self) {
        // Runtime shutdown can cancel the task. Keep the claim until after the
        // operation's future is dropped, including its rollback resources.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(self.work.take());
        }));
        drop(self.claim.take());
    }
}

impl BackgroundLaunchClaim {
    /// Run admitted publication independently of its awaiting client. Shutdown
    /// observes completion only after this future has released its resources.
    pub async fn run_owned<T, F>(self, operation: F) -> Result<T, BgForkError>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, BgForkError>> + Send + 'static,
    {
        let (reply, result) = tokio::sync::oneshot::channel();
        drop(tokio::spawn(OwnedLaunch {
            work: Some(Box::pin(
                std::panic::AssertUnwindSafe(operation).catch_unwind(),
            )),
            claim: Some(self),
            reply: Some(reply),
        }));
        result
            .await
            .map_err(|_| BgForkError::Dispatch("background launch owner stopped".into()))?
    }
}

impl Drop for BackgroundLaunchClaim {
    fn drop(&mut self) {
        let mut state = self
            .lifecycle
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if self.started && !self.settled {
            state
                .failures
                .push("background launch stopped before ownership settled".into());
        }
        state.active = state
            .active
            .checked_sub(1)
            .expect("background launch claim underflow");
        drop(state);
        self.lifecycle.changed.notify_waiters();
    }
}

#[cfg(test)]
#[path = "bg_session_lifecycle_tests.rs"]
mod tests;
