//! Own prewarm completion independently of clients waiting for host shutdown.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::task::AbortHandle;

/// A completed drain confirms all registered prewarm futures were dropped.
/// Historical failures remain visible without blocking subsequent cleanup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupPrewarmDrainReport {
    pub diagnostics: Vec<String>,
}

#[derive(Default)]
struct State {
    closed: bool,
    generation: u64,
    current: Option<u64>,
    tasks: HashMap<u64, AbortHandle>,
    diagnostics: Vec<String>,
}

#[derive(Default)]
struct Owner {
    state: Mutex<State>,
    changed: tokio::sync::Notify,
}

/// Spawn and permanent close share one lock. Replaced and aborted tasks remain
/// registered until independent join owners confirm their futures were dropped.
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
        if let Some(previous) = state.current.take().and_then(|id| state.tasks.get(&id)) {
            previous.abort();
        }
        state.generation = state
            .generation
            .checked_add(1)
            .expect("prewarm generation overflow");
        let generation = state.generation;
        let task = tokio::spawn(future);
        state.tasks.insert(generation, task.abort_handle());
        state.current = Some(generation);
        let owner = self.owner.clone();
        // This owner survives a cancelled waiter and releases its registration
        // only after Tokio has dropped the actual work future.
        tokio::spawn(async move {
            let result = task.await;
            let mut state = owner.state.lock().unwrap_or_else(|p| p.into_inner());
            state.tasks.remove(&generation);
            if state.current == Some(generation) {
                state.current = None;
            }
            if let Err(error) = result {
                if !error.is_cancelled() {
                    state
                        .diagnostics
                        .push(format!("startup prewarm failed: {error}"));
                }
            }
            drop(state);
            owner.changed.notify_waiters();
        });
    }

    /// Ordinary turn, clear, or resume cancellation allows later prewarming.
    pub(crate) fn abort_current(&self) {
        let mut state = self.owner.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(task) = state.current.take().and_then(|id| state.tasks.get(&id)) {
            task.abort();
        }
    }

    pub(crate) fn close(&self) {
        let mut state = self.owner.state.lock().unwrap_or_else(|p| p.into_inner());
        state.closed = true;
        state.current = None;
        for task in state.tasks.values() {
            task.abort();
        }
    }

    pub(crate) async fn close_and_drain(&self) -> StartupPrewarmDrainReport {
        self.close();
        loop {
            let changed = self.owner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.owner.state.lock().unwrap_or_else(|p| p.into_inner());
                if state.tasks.is_empty() {
                    return StartupPrewarmDrainReport {
                        diagnostics: state.diagnostics.clone(),
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
