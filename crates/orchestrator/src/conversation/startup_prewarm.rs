//! Track every prewarm until its work future has actually been dropped.

use futures::FutureExt;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::task::AbortHandle;

#[derive(Default)]
struct State {
    closed: bool,
    generation: u64,
    current: Option<u64>,
    tasks: HashMap<u64, TaskEntry>,
    failures: Vec<String>,
}

#[derive(Default)]
struct TaskEntry {
    abort: Option<AbortHandle>,
    abort_requested: bool,
}

impl TaskEntry {
    fn abort(&mut self) {
        self.abort_requested = true;
        if let Some(handle) = &self.abort {
            handle.abort();
        }
    }
}

#[derive(Default)]
struct Owner {
    state: Mutex<State>,
    changed: tokio::sync::Notify,
}

/// The guard belongs to the work task itself, so even destruction of the
/// originating runtime drops the work before removing its registered claim.
type PanicPayload = Box<dyn std::any::Any + Send>;
type CaughtWorkFuture = Pin<Box<dyn Future<Output = Result<(), PanicPayload>> + Send>>;

struct TrackedPrewarm {
    work: Option<CaughtWorkFuture>,
    owner: Arc<Owner>,
    generation: u64,
}

impl Future for TrackedPrewarm {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let result = match self.work.as_mut().expect("prewarm work").as_mut().poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(result) => result,
        };
        drop(self.work.take());
        if result.is_err() {
            self.owner
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .failures
                .push("startup prewarm task panicked".into());
        }
        Poll::Ready(())
    }
}

impl Drop for TrackedPrewarm {
    fn drop(&mut self) {
        let drop_failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(self.work.take());
        }))
        .is_err();
        let mut state = self.owner.state.lock().unwrap_or_else(|p| p.into_inner());
        if drop_failed || std::thread::panicking() {
            state
                .failures
                .push("startup prewarm task panicked while releasing work".into());
        }
        state.tasks.remove(&self.generation);
        if state.current == Some(self.generation) {
            state.current = None;
        }
        drop(state);
        self.owner.changed.notify_waiters();
    }
}

/// Spawns and permanent close share one lock. Replaced/aborted tasks remain
/// registered until their own guards confirm their futures were dropped.
#[derive(Default)]
pub(crate) struct StartupPrewarm {
    owner: Arc<Owner>,
}

impl StartupPrewarm {
    pub(crate) fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut state = self.owner.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.closed {
            drop(state);
            return;
        }
        if let Some(previous) = state.current.take().and_then(|id| state.tasks.get_mut(&id)) {
            previous.abort();
        }
        state.generation = state
            .generation
            .checked_add(1)
            .expect("prewarm generation overflow");
        let generation = state.generation;
        state.tasks.insert(generation, TaskEntry::default());
        state.current = Some(generation);
        drop(state);
        let task = tokio::spawn(TrackedPrewarm {
            work: Some(Box::pin(
                std::panic::AssertUnwindSafe(future).catch_unwind(),
            )),
            owner: self.owner.clone(),
            generation,
        });
        let mut state = self.owner.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = state.tasks.get_mut(&generation) {
            if entry.abort_requested {
                task.abort();
            }
            entry.abort = Some(task.abort_handle());
        }
        // Dropping a JoinHandle detaches it. TrackedPrewarm itself removes the
        // claim after its future is dropped, including runtime shutdown.
        drop(task);
    }

    /// Ordinary turn/clear/resume cancellation does not close future admission.
    pub(crate) fn abort_current(&self) {
        let mut state = self.owner.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(task) = state.current.take().and_then(|id| state.tasks.get_mut(&id)) {
            task.abort();
        }
    }

    pub(crate) fn close(&self) {
        let mut state = self.owner.state.lock().unwrap_or_else(|p| p.into_inner());
        state.closed = true;
        state.current = None;
        for task in state.tasks.values_mut() {
            task.abort();
        }
    }

    pub(crate) async fn close_and_drain(&self) -> Result<(), String> {
        self.close();
        loop {
            let changed = self.owner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.owner.state.lock().unwrap_or_else(|p| p.into_inner());
                if state.tasks.is_empty() {
                    return if state.failures.is_empty() {
                        Ok(())
                    } else {
                        Err(state.failures.join("; "))
                    };
                }
            }
            changed.await;
        }
    }

    #[cfg(test)]
    pub(crate) fn is_running(&self) -> bool {
        self.owner
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .current
            .is_some()
    }
}

impl Drop for StartupPrewarm {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
#[path = "startup_prewarm_tests.rs"]
mod tests;
