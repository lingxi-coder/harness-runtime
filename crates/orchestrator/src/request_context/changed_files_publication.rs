//! Protect Read state publication until its stable notices are retained.
//! Initial ingress admits them at its batch boundary; continuations publish
//! them directly before this scope completes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Default)]
pub(crate) struct ChangedFilesPublication {
    read_started: AtomicBool,
    completed: AtomicBool,
    completed_notify: tokio::sync::Notify,
}

impl ChangedFilesPublication {
    pub(crate) async fn wait_for_completion(&self) {
        if !self.completed.load(Ordering::Acquire) {
            // There is one cancelling preparation waiter. notify_one retains
            // a permit if publication finishes before this future is polled.
            self.completed_notify.notified().await;
        }
    }
}

pub(crate) struct ChangedFilesPublicationGuard {
    publication: Arc<ChangedFilesPublication>,
    previous: Option<Arc<ChangedFilesPublication>>,
}

impl Drop for ChangedFilesPublicationGuard {
    fn drop(&mut self) {
        self.publication.completed.store(true, Ordering::Release);
        self.publication.completed_notify.notify_one();
        let _ = super::REQUEST_CONTEXT.try_with(|context| {
            let mut batch = context
                .ingress_batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if batch
                .changed_files_publication
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &self.publication))
            {
                batch.changed_files_publication = self.previous.take();
            }
        });
    }
}

/// A scope starts unarmed: stat, permission checks and waiting for the session
/// remain cancelable until an actual Read can publish shared cache state.
pub(crate) fn changed_files_publication_scope() -> Option<ChangedFilesPublicationGuard> {
    super::REQUEST_CONTEXT
        .try_with(|context| {
            let publication = Arc::new(ChangedFilesPublication::default());
            let previous = context
                .ingress_batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .changed_files_publication
                .replace(publication.clone());
            ChangedFilesPublicationGuard {
                publication,
                previous,
            }
        })
        .ok()
}

pub(crate) fn mark_changed_files_publication_started() {
    let _ = super::REQUEST_CONTEXT.try_with(|context| {
        if let Some(publication) = &context
            .ingress_batch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .changed_files_publication
        {
            publication.read_started.store(true, Ordering::Release);
        }
    });
}

pub(crate) fn pending_changed_files_publication() -> Option<Arc<ChangedFilesPublication>> {
    super::REQUEST_CONTEXT
        .try_with(|context| {
            context
                .ingress_batch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .changed_files_publication
                .clone()
        })
        .ok()
        .flatten()
        .filter(|publication| {
            publication.read_started.load(Ordering::Acquire)
                && !publication.completed.load(Ordering::Acquire)
        })
}
