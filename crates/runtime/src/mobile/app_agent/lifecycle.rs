//! Completion notification shared by cancel, configuration refresh and version handoff.
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

pub(super) async fn wait_for_completion(done: &Notify, finished: &AtomicBool) {
    loop {
        let notified = done.notified();
        tokio::pin!(notified);
        // notify_waiters does not retain a permit. Register before reading the
        // completion flag so completion cannot land between the read and await.
        notified.as_mut().enable();
        if finished.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[tokio::test]
    async fn already_finished_does_not_require_a_retained_notification() {
        let done = Notify::new();
        let finished = AtomicBool::new(true);
        done.notify_waiters();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_for_completion(&done, &finished),
        )
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn completion_during_wait_registration_never_loses_a_wakeup() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for i in 0..1000 {
                let state = Arc::new((Notify::new(), AtomicBool::new(false)));
                let other = state.clone();
                let waiter =
                    tokio::spawn(async move { wait_for_completion(&other.0, &other.1).await });
                if i % 2 == 0 {
                    tokio::task::yield_now().await;
                }
                state.1.store(true, Ordering::Release);
                state.0.notify_waiters();
                waiter.await.unwrap();
            }
        })
        .await
        .unwrap();
    }
}
