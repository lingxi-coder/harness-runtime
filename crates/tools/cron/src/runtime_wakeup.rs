//! Runtime-neutral dynamic-loop timer used by local host composition roots.
use crate::{LoopRuntime, WakeupScheduler};
use async_trait::async_trait;
use platform_api::{BackgroundTaskHandle, RuntimeSpawner};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Identity captured when a dynamic task is armed, before its timer can fire.
#[derive(Clone, Debug)]
pub struct WakeupTask {
    pub fire_id: protocol::MessageId,
    pub task_id: String,
    pub cron: String,
    pub display_prompt: String,
    pub task_kind_loop: bool,
}
impl WakeupTask {
    pub fn command_id(&self) -> String {
        format!(
            "{}-{}-{}",
            if self.task_kind_loop {
                "loop-wakeup"
            } else {
                "cron-fire"
            },
            self.task_id,
            self.fire_id.as_uuid()
        )
    }
    pub fn scheduled(fire: &cron::scheduler::SessionCronFire) -> Self {
        let mut task = Self::new(Duration::ZERO, &fire.prompt);
        task.task_id = fire.id.clone();
        task.cron = fire.cron.clone();
        task.task_kind_loop = false;
        task
    }
    pub fn lines(
        &self,
        now_ms: u64,
        streak: Option<(u32, std::time::SystemTime)>,
    ) -> (String, Option<String>) {
        if self.task_kind_loop {
            crate::loop_wakeup_lines(now_ms, streak)
        } else {
            (
                format!(
                    "Running scheduled task ({})",
                    cron::short_local_timestamp(now_ms)
                ),
                None,
            )
        }
    }
    pub fn new(delay: Duration, prompt: &str) -> Self {
        Self::at(std::time::SystemTime::now() + delay, prompt)
    }
    fn at(target: std::time::SystemTime, prompt: &str) -> Self {
        use rand::Rng;
        let seconds = target
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let local = seconds + cron::schedule::local_offset_seconds(seconds);
        let minute = local.rem_euclid(3600) / 60;
        let hour = local.rem_euclid(86400) / 3600;
        // Native Math.floor(Math.random() * 4294967295), padded to eight hex digits.
        let task_id = format!("{:08x}", rand::rng().random_range(0..u32::MAX));
        let display_prompt = if crate::is_loop_default_sentinel(prompt) {
            if crate::is_loop_file_sentinel(prompt) {
                "/loop (loop.md)"
            } else {
                "/loop"
            }
        } else {
            prompt
        }
        .to_owned();
        Self {
            fire_id: protocol::MessageId::new(),
            task_id,
            cron: format!("{minute} {hour} * * *"),
            display_prompt,
            task_kind_loop: true,
        }
    }
}

/// Host-owned delivery boundary. Keep raw prompt identity until queue drain.
#[async_trait]
pub trait WakeupDelivery: Send + Sync {
    /// Wait for the host's idle boundary, announce the fire, then enqueue it.
    async fn deliver(&self, command_id: &str, prompt: String, reason: String, task: WakeupTask);
    /// Remove already-enqueued dynamic wakeups and return their raw prompts.
    async fn cancel_queued(&self) -> Vec<String>;
}

struct Pending {
    id: String,
    prompt: String,
    handle: BackgroundTaskHandle,
    cancel: tokio::sync::watch::Sender<bool>,
}

/// One-shot scheduler with cancellation covering delivery, not only sleeping.
pub struct RuntimeWakeupScheduler {
    runtime: Arc<dyn RuntimeSpawner>,
    state: Arc<LoopRuntime>,
    delivery: Arc<dyn WakeupDelivery>,
    pending: Arc<Mutex<Vec<Pending>>>,
}
impl RuntimeWakeupScheduler {
    /// Bind a host runtime and its session-scoped delivery adapter.
    pub fn new(
        runtime: Arc<dyn RuntimeSpawner>,
        state: Arc<LoopRuntime>,
        delivery: Arc<dyn WakeupDelivery>,
    ) -> Self {
        Self {
            runtime,
            state,
            delivery,
            pending: Arc::new(Mutex::new(Vec::new())),
        }
    }
}
impl Drop for RuntimeWakeupScheduler {
    fn drop(&mut self) {
        for entry in self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            let _ = entry.cancel.send(true);
        }
    }
}
#[async_trait]
impl WakeupScheduler for RuntimeWakeupScheduler {
    async fn schedule(&self, delay: Duration, prompt: String, reason: String) {
        let task = WakeupTask::new(delay, &prompt);
        let id = task.command_id();
        let task_id = id.clone();
        let task_prompt = prompt.clone();
        let delivery = self.delivery.clone();
        let runtime = self.runtime.clone();
        let pending = self.pending.clone();
        let (cancel, mut cancelled) = tokio::sync::watch::channel(false);
        let (registered, registration) = tokio::sync::oneshot::channel();
        let handle = self.runtime.spawn("loop-wakeup", Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancelled.changed() => {},
                _ = async {
                    if registration.await.is_err() { return; }
                    runtime.sleep(delay).await;
                    delivery.deliver(&task_id, task_prompt, reason, task).await;
                    pending.lock().unwrap_or_else(|e| e.into_inner()).retain(|p| p.id != task_id);
                } => {},
            }
        })).await;
        if let Ok(handle) = handle {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Pending {
                    id,
                    prompt,
                    handle,
                    cancel,
                });
            let _ = registered.send(());
        }
    }
    async fn cancel_pending(&self) -> Vec<String> {
        let pending = std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        for entry in &pending {
            let _ = entry.cancel.send(true);
        }
        let mut prompts = Vec::new();
        for entry in pending {
            let _ = self.runtime.cancel(&entry.handle).await;
            prompts.push(entry.prompt);
        }
        for prompt in self.delivery.cancel_queued().await {
            // A firing task can be present in both registries during the
            // enqueue -> pending-retirement handoff. A dynamic loop has only
            // one pending wakeup per prompt; count that handoff once.
            if !prompts.contains(&prompt) {
                prompts.push(prompt);
            }
        }
        prompts
    }
    fn loop_runtime(&self) -> Option<Arc<LoopRuntime>> {
        Some(self.state.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::RuntimeError;
    #[test]
    fn scheduled_identity_uses_target_clock_and_exact_default_sentinels() {
        let seconds = 1_700_000_040;
        let target = std::time::UNIX_EPOCH + Duration::from_secs(seconds);
        let task = WakeupTask::at(target, "<<loop.md-dynamic>>");
        let local = seconds as i64 + cron::schedule::local_offset_seconds(seconds as i64);
        assert_eq!(
            task.cron,
            format!(
                "{} {} * * *",
                local.rem_euclid(3600) / 60,
                local.rem_euclid(86400) / 3600
            )
        );
        assert_eq!(task.display_prompt, "/loop (loop.md)");
        assert_eq!(task.task_id.len(), 8);
        assert!(u32::from_str_radix(&task.task_id, 16).unwrap() < u32::MAX);
        assert_eq!(
            WakeupTask::at(target, "<<autonomous-loop-dynamic>>").display_prompt,
            "/loop"
        );
        assert_eq!(
            WakeupTask::at(target, " <<loop.md-dynamic>> ").display_prompt,
            " <<loop.md-dynamic>> "
        );
    }
    struct TestRuntime;
    #[async_trait]
    impl RuntimeSpawner for TestRuntime {
        async fn spawn(
            &self,
            name: &str,
            task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<BackgroundTaskHandle, RuntimeError> {
            tokio::spawn(task);
            Ok(BackgroundTaskHandle {
                task_name: name.into(),
                task_id: 0,
            })
        }
        async fn sleep(&self, duration: Duration) {
            tokio::time::sleep(duration).await;
        }
        // Proves logical cancellation also works without runtime abort support.
        async fn cancel(&self, _: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
            Ok(())
        }
    }
    struct Delivery {
        entered: tokio::sync::Semaphore,
        release: tokio::sync::Semaphore,
        queued: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl WakeupDelivery for Delivery {
        async fn deliver(&self, id: &str, prompt: String, _: String, _: WakeupTask) {
            assert!(id.starts_with("loop-wakeup-"));
            self.entered.add_permits(1);
            self.release.acquire().await.unwrap().forget();
            self.queued.lock().unwrap().push(prompt);
        }
        async fn cancel_queued(&self) -> Vec<String> {
            std::mem::take(&mut *self.queued.lock().unwrap())
        }
    }
    fn fixture() -> (RuntimeWakeupScheduler, Arc<Delivery>) {
        let delivery = Arc::new(Delivery {
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
            queued: Mutex::new(vec![]),
        });
        (
            RuntimeWakeupScheduler::new(
                Arc::new(TestRuntime),
                Arc::new(LoopRuntime::default()),
                delivery.clone(),
            ),
            delivery,
        )
    }
    #[tokio::test]
    async fn cancellation_covers_blocked_publication_without_runtime_abort() {
        let (scheduler, delivery) = fixture();
        scheduler
            .schedule(
                Duration::ZERO,
                "<<autonomous-loop-dynamic>>".into(),
                "test".into(),
            )
            .await;
        delivery.entered.acquire().await.unwrap().forget();
        assert_eq!(
            scheduler.cancel_pending().await,
            vec!["<<autonomous-loop-dynamic>>"]
        );
        delivery.release.add_permits(1);
        tokio::task::yield_now().await;
        assert!(delivery.queued.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn fired_wakeup_keeps_raw_identity_and_can_be_cancelled_while_queued() {
        let (scheduler, delivery) = fixture();
        delivery.release.add_permits(1);
        scheduler
            .schedule(Duration::ZERO, " /original ".into(), "test".into())
            .await;
        delivery.entered.acquire().await.unwrap().forget();
        tokio::task::yield_now().await;
        assert_eq!(*delivery.queued.lock().unwrap(), vec![" /original "]);
        assert_eq!(scheduler.cancel_pending().await, vec![" /original "]);
        assert!(delivery.queued.lock().unwrap().is_empty());
    }
}
