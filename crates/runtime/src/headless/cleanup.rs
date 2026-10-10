//! Retains the resource graph after an incomplete shutdown report. Dropping a
//! caller's diagnostic handle never abandons accepted persistence work.
use std::sync::Arc;

use crate::desktop::{DesktopRuntime, DesktopSessionShutdownReport};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadlessCleanupStatus {
    pub complete: bool,
    pub errors: Vec<String>,
}

impl From<DesktopSessionShutdownReport> for HeadlessCleanupStatus {
    fn from(report: DesktopSessionShutdownReport) -> Self {
        Self {
            complete: report.complete,
            errors: report.errors,
        }
    }
}

/// Observe retained cleanup without owning its lifetime. The cleanup task
/// keeps the runtime alive until its existing lifecycle confirms completion.
#[derive(Clone, Debug)]
pub struct HeadlessCleanup {
    status: tokio::sync::watch::Receiver<HeadlessCleanupStatus>,
}

impl HeadlessCleanup {
    pub fn status(&self) -> HeadlessCleanupStatus {
        self.status.borrow().clone()
    }

    pub async fn wait(&mut self) -> HeadlessCleanupStatus {
        loop {
            let status = self.status.borrow_and_update().clone();
            if status.complete || self.status.changed().await.is_err() {
                return status;
            }
        }
    }
}

#[async_trait::async_trait]
pub(crate) trait CleanupOwner: Send + Sync + 'static {
    async fn drain(&self) -> DesktopSessionShutdownReport;
}

#[async_trait::async_trait]
impl CleanupOwner for DesktopRuntime {
    async fn drain(&self) -> DesktopSessionShutdownReport {
        self.session_lifecycle.shutdown_and_drain().await
    }
}

#[async_trait::async_trait]
impl CleanupOwner for super::HeadlessRuntime {
    async fn drain(&self) -> DesktopSessionShutdownReport {
        self.session_lifecycle.shutdown_and_drain().await
    }
}

pub(crate) fn retain_cleanup<T: CleanupOwner>(
    owner: Arc<T>,
    report: DesktopSessionShutdownReport,
) -> HeadlessCleanup {
    let (status, receiver) = tokio::sync::watch::channel(report.into());
    tokio::spawn(async move {
        let mut retry_delay = std::time::Duration::from_millis(100);
        loop {
            tokio::time::sleep(retry_delay).await;
            let report = owner.drain().await;
            let complete = report.complete;
            status.send_replace(report.into());
            if complete {
                break;
            }
            retry_delay = (retry_delay * 2).min(std::time::Duration::from_secs(2));
        }
    });
    HeadlessCleanup { status: receiver }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct RetryOwner {
        attempts: AtomicUsize,
        released: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl CleanupOwner for RetryOwner {
        async fn drain(&self) -> DesktopSessionShutdownReport {
            let complete = self.attempts.fetch_add(1, Ordering::SeqCst) > 0;
            DesktopSessionShutdownReport {
                complete,
                errors: if complete {
                    vec![]
                } else {
                    vec!["retry".into()]
                },
                ..Default::default()
            }
        }
    }

    impl Drop for RetryOwner {
        fn drop(&mut self) {
            self.released.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn cleanup_retains_owner_until_a_later_drain_completes() {
        let released = Arc::new(AtomicBool::new(false));
        let owner = Arc::new(RetryOwner {
            attempts: AtomicUsize::new(0),
            released: released.clone(),
        });
        let weak = Arc::downgrade(&owner);
        let mut handle = retain_cleanup(owner, DesktopSessionShutdownReport::default());
        assert!(weak.upgrade().is_some());
        let report = tokio::time::timeout(std::time::Duration::from_secs(2), handle.wait())
            .await
            .unwrap();
        assert!(report.complete);
        assert!(report.errors.is_empty());
        tokio::task::yield_now().await;
        assert!(released.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn dropping_diagnostic_handle_does_not_drop_cleanup_owner() {
        let released = Arc::new(AtomicBool::new(false));
        let owner = Arc::new(RetryOwner {
            attempts: AtomicUsize::new(0),
            released: released.clone(),
        });
        drop(retain_cleanup(
            owner,
            DesktopSessionShutdownReport::default(),
        ));
        assert!(!released.load(Ordering::SeqCst));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !released.load(Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
}
