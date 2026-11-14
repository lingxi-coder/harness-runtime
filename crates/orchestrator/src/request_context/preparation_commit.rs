//! Short preparation transactions which must settle after their first effect.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Default)]
pub(crate) struct PreparationCommit {
    completed: AtomicBool,
    completed_notify: tokio::sync::Notify,
}

impl PreparationCommit {
    pub(crate) async fn wait_for_completion(&self) {
        if !self.completed.load(Ordering::Acquire) {
            self.completed_notify.notified().await;
        }
    }
}

pub(crate) struct PreparationCommitGuard {
    commit: Arc<PreparationCommit>,
    previous: Option<Arc<PreparationCommit>>,
}

impl Drop for PreparationCommitGuard {
    fn drop(&mut self) {
        self.commit.completed.store(true, Ordering::Release);
        self.commit.completed_notify.notify_one();
        let _ = super::REQUEST_CONTEXT.try_with(|context| {
            let mut batch = context
                .ingress_batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if batch
                .preparation_commit
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &self.commit))
            {
                batch.preparation_commit = self.previous.take();
            }
        });
    }
}

/// Call immediately before a preparation phase starts mutating shared state.
/// The guard covers only that phase, never the query or its provider request.
pub(crate) fn preparation_commit_scope() -> Option<PreparationCommitGuard> {
    super::REQUEST_CONTEXT
        .try_with(|context| {
            let commit = Arc::new(PreparationCommit::default());
            let previous = context
                .ingress_batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .preparation_commit
                .replace(commit.clone());
            PreparationCommitGuard { commit, previous }
        })
        .ok()
}

pub(crate) fn pending_preparation_commit() -> Option<Arc<PreparationCommit>> {
    super::REQUEST_CONTEXT
        .try_with(|context| {
            context
                .ingress_batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .preparation_commit
                .clone()
        })
        .ok()
        .flatten()
        .filter(|commit| !commit.completed.load(Ordering::Acquire))
}
