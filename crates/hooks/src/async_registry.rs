//! Background async hook registry — tracks long-running hooks spawned via
//! [`crate::RuntimeSpawner`] so their completions can be collected on a
//! dedicated channel.
//!
//! This is the config-`async` (`blocking == false`) backgrounding path,
//! mirroring claude-code `executeInBackground` (`utils/hooks.ts:995-1030`):
//! a hook flagged non-blocking is detached from the in-flight action so the
//! engine proceeds immediately, and its eventual result folds back through a
//! completion channel rather than gating the originating turn.
//!
//! Runtime first-line `{"async":true}` detection (`hooks.ts:1117-1166`) uses
//! the process runner's streaming detection seam and registers the remaining
//! output in this same registry, so configured and runtime-selected async hooks
//! share timeout, completion, persistence, and re-wake behavior.

use crate::attachment::HookPublicationGuard;
use crate::response::{ExactHookText, HookOutcome, HookResult};
use lingxi_core::host::{BackgroundTaskHandle, RuntimeError, RuntimeSpawner};
use lingxi_core::types::HookId;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};

/// Default async-hook timeout (15s) — matches claude-code
/// `registerPendingAsyncHook` (`utils/hooks/AsyncHookRegistry.ts:51`):
/// `const timeout = asyncResponse.asyncTimeout || 15000`.
pub const DEFAULT_ASYNC_HOOK_TIMEOUT_MS: u64 = 15_000;

/// Boxed `'static` future producing the eventual [`HookResult`] of a
/// backgrounded hook. The executor builds one of these (capturing owned
/// `Arc`s for the relevant arm) and hands it to [`AsyncHookRegistry::spawn`].
pub type HookWork = Pin<Box<dyn Future<Output = HookResult> + Send + 'static>>;

/// Finalizer invoked exactly once with the registry's winning result,
/// including a timeout synthesized by the registry itself.
pub type HookCompletion = Box<
    dyn FnOnce(HookResult) -> Pin<Box<dyn Future<Output = HookResult> + Send + 'static>>
        + Send
        + 'static,
>;

/// A completed async-hook result retains the generation that admitted it until
/// the session consumer drains it. Producer-side fencing alone cannot reject a
/// completion already buffered when reset starts.
#[derive(Clone)]
pub struct HookCompletionEnvelope {
    pub hook_id: HookId,
    pub result: HookResult,
    pub hook_event: Option<String>,
    pub publication_guard: Option<Arc<dyn HookPublicationGuard>>,
}

impl HookCompletionEnvelope {
    /// Whether this completion may still be delivered to a later prompt.
    #[must_use]
    pub fn is_current(&self) -> bool {
        self.publication_guard
            .as_ref()
            .is_none_or(|guard| guard.is_current())
    }
}

/// Registry of currently-running non-blocking hooks.
///
/// Each `spawn` call hands the hook off to the runtime spawner and stores the
/// resulting handle so the engine can cancel or join it later. Completed hooks
/// publish `(HookId, HookResult, hook event)` on `completion_tx` so the engine
/// can fold the result back into the originating session with provenance.
pub struct AsyncHookRegistry {
    runtime: Arc<dyn RuntimeSpawner>,
    in_flight: Arc<Mutex<HashMap<HookId, BackgroundTaskHandle>>>,
    completion_tx: mpsc::Sender<HookCompletionEnvelope>,
}

impl AsyncHookRegistry {
    /// Build a new registry backed by `runtime` and the supplied completion
    /// channel sender.
    #[must_use]
    pub fn new(
        runtime: Arc<dyn RuntimeSpawner>,
        completion_tx: mpsc::Sender<HookCompletionEnvelope>,
    ) -> Self {
        Self {
            runtime,
            in_flight: Arc::new(Mutex::new(HashMap::new())),
            completion_tx,
        }
    }

    /// Spawn a non-blocking hook into the background.
    ///
    /// `work` is the future that actually executes the hook (built by the
    /// executor for the appropriate arm); `hook_id` keys the in-flight entry
    /// and the completion tuple; `async_timeout` bounds the wall-clock the
    /// hook may run (falling back to [`DEFAULT_ASYNC_HOOK_TIMEOUT_MS`] when
    /// `None`, matching claude-code's `asyncTimeout || 15000`).
    ///
    /// The spawned task races `work` against `runtime.sleep(timeout)`. On
    /// expiry the completion carries a [`HookOutcome::Timeout`] result; on
    /// normal completion it carries the hook's own [`HookResult`]. Either way
    /// the entry is removed from `in_flight` and the result is published on
    /// `completion_tx` (best-effort — a closed receiver is ignored).
    ///
    /// Returns the [`BackgroundTaskHandle`] of the spawned task so callers can
    /// later `cancel` it.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the runtime refuses the spawn (e.g. it is
    /// shutting down).
    pub async fn spawn(
        &self,
        hook_id: HookId,
        async_timeout: Option<Duration>,
        work: HookWork,
        publication_guard: Option<Arc<dyn HookPublicationGuard>>,
    ) -> Result<BackgroundTaskHandle, RuntimeError> {
        self.spawn_with_completion(hook_id, async_timeout, work, None, publication_guard)
            .await
    }

    /// Spawn a non-blocking hook and run `completion` with the final result.
    ///
    /// Unlike code inside `work`, this finalizer also runs when the registry's
    /// timeout wins the race. It is intended for side effects such as
    /// transcript persistence that must observe every terminal outcome.
    ///
    /// # Errors
    /// Returns [`RuntimeError`] if the runtime refuses the spawn.
    pub async fn spawn_with_completion(
        &self,
        hook_id: HookId,
        async_timeout: Option<Duration>,
        work: HookWork,
        completion: Option<HookCompletion>,
        publication_guard: Option<Arc<dyn HookPublicationGuard>>,
    ) -> Result<BackgroundTaskHandle, RuntimeError> {
        self.spawn_with_completion_and_rewake(
            hook_id,
            async_timeout,
            work,
            completion,
            None,
            publication_guard,
        )
        .await
    }

    /// Spawn a non-blocking hook and optionally mark its terminal result for a
    /// model re-wake. Applying the re-wake metadata after the timeout race is
    /// intentional: timeouts must wake the session too, with a bounded context
    /// message, rather than publishing an empty completion.
    pub async fn spawn_with_completion_and_rewake(
        &self,
        hook_id: HookId,
        async_timeout: Option<Duration>,
        work: HookWork,
        completion: Option<HookCompletion>,
        rewake_message: Option<String>,
        publication_guard: Option<Arc<dyn HookPublicationGuard>>,
    ) -> Result<BackgroundTaskHandle, RuntimeError> {
        self.spawn_with_completion_and_rewake_for_event(
            hook_id,
            async_timeout,
            work,
            completion,
            rewake_message,
            None,
            publication_guard,
        )
        .await
    }

    /// Carry the originating settings-hook event with this individual run.
    /// Hook IDs can run concurrently, so provenance belongs on the completion
    /// record rather than in a side map keyed by the hook definition.
    pub async fn spawn_with_completion_and_rewake_for_event(
        &self,
        hook_id: HookId,
        async_timeout: Option<Duration>,
        work: HookWork,
        completion: Option<HookCompletion>,
        rewake_message: Option<String>,
        hook_event: Option<String>,
        publication_guard: Option<Arc<dyn HookPublicationGuard>>,
    ) -> Result<BackgroundTaskHandle, RuntimeError> {
        let timeout =
            async_timeout.unwrap_or_else(|| Duration::from_millis(DEFAULT_ASYNC_HOOK_TIMEOUT_MS));
        // One clone races the timeout inside the task; a second drives the
        // spawn below (the first is moved into the `async move` block).
        let timeout_runtime = self.runtime.clone();
        let in_flight = self.in_flight.clone();
        let completion_tx = self.completion_tx.clone();

        let task: Pin<Box<dyn Future<Output = ()> + Send + 'static>> = Box::pin(async move {
            // Race the hook against its timeout via the runtime's `sleep`.
            // `tokio::select!` polls both arms; whichever resolves first wins.
            let mut result = tokio::select! {
                biased;
                r = work => r,
                () = timeout_runtime.sleep(timeout) => timeout_result(),
            };
            if let Some(message) = rewake_message.as_deref() {
                mark_async_rewake(&mut result, message);
            }
            // Drop the in-flight entry before publishing so a draining engine
            // never observes a completed-but-still-tracked hook.
            in_flight.lock().await.remove(&hook_id);
            // Durable finalization and Native Fse output normalization are an
            // admitted commit: reset waits until the file/attachment pair is
            // complete, and the exact normalized response that follows is the
            // one consumed by the later async-hook reminder. The channel send
            // remains separately cancellable so receiver backpressure cannot
            // hold reset.
            if let Some(guard) = publication_guard.as_ref() {
                if let Some(completion) = completion {
                    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
                    let mutation = async move {
                        let _ = result_tx.send(completion(result).await);
                    };
                    if !guard.commit_if_current(Box::pin(mutation)).await {
                        return;
                    }
                    let Ok(completed) = result_rx.await else {
                        return;
                    };
                    result = completed;
                }
            } else {
                if let Some(completion) = completion {
                    result = completion(result).await;
                }
            }
            // The envelope retains the guard so a receiver can reject it if
            // reset happens after channel admission but before consumption.
            let envelope = HookCompletionEnvelope {
                hook_id,
                result,
                hook_event,
                publication_guard: publication_guard.clone(),
            };
            // Best-effort publish: a closed receiver (engine torn down) is not
            // an error for a fire-and-forget hook. Reset cancels a blocked send.
            let send = async move {
                let _ = completion_tx.send(envelope).await;
            };
            if let Some(guard) = publication_guard.as_ref() {
                let _ = guard.publish_if_current(Box::pin(send)).await;
            } else {
                send.await;
            }
        });

        // Hold the in-flight lock across spawn + insert so the spawned task's
        // own removal (which takes the same lock) can never run before this
        // insert — otherwise a fast hook could remove-then-be-re-inserted,
        // leaking a stale entry on a multi-threaded runtime.
        let mut guard = self.in_flight.lock().await;
        let handle = self.runtime.spawn("async_hook", task).await?;
        guard.insert(hook_id, handle.clone());
        drop(guard);
        Ok(handle)
    }

    /// Number of hooks currently tracked as in-flight. Primarily for tests and
    /// diagnostics.
    #[must_use]
    pub async fn in_flight_len(&self) -> usize {
        self.in_flight.lock().await.len()
    }

    /// Whether `hook_id` is currently tracked as in-flight.
    #[must_use]
    pub async fn is_in_flight(&self, hook_id: HookId) -> bool {
        self.in_flight.lock().await.contains_key(&hook_id)
    }

    /// Cancel a previously-spawned background hook, dropping its in-flight
    /// entry. No-op if the hook already completed or was never tracked.
    ///
    /// # Errors
    /// Propagates a [`RuntimeError`] from the runtime's `cancel`.
    pub async fn cancel(&self, hook_id: HookId) -> Result<(), RuntimeError> {
        let handle = self.in_flight.lock().await.remove(&hook_id);
        if let Some(handle) = handle {
            self.runtime.cancel(&handle).await?;
        }
        Ok(())
    }
}

const MAX_REWAKE_MESSAGE_CHARS: usize = 10_000;

fn mark_async_rewake(result: &mut HookResult, configured: &str) {
    let raw = if configured.trim().is_empty() {
        "An asynchronous hook completed. Continue from its result."
    } else {
        configured.trim()
    };
    let message = ExactHookText::from_text(raw).truncate_well_formed(MAX_REWAKE_MESSAGE_CHARS);
    let response = result.response.get_or_insert_with(Default::default);
    response.additional_context = Some(match response.additional_context.take() {
        Some(existing) if !existing.trim_js().is_empty() => {
            let mut combined = existing;
            combined.push_text("\n");
            combined.push(&message);
            combined
        }
        _ => message,
    });
    response.async_rewake = true;
}

/// The [`HookResult`] published when an async hook exceeds its timeout. Carries
/// [`HookOutcome::Timeout`] with no parsed response so a draining engine never
/// mistakes a timed-out hook for a `Block`.
fn timeout_result() -> HookResult {
    HookResult {
        outcome: HookOutcome::Timeout,
        stdout: String::new(),
        stderr: "async hook timed out".to_string(),
        exit_code: None,
        response: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::HookOutcome;
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use tokio::sync::Notify;

    /// Tokio-backed [`RuntimeSpawner`] for these unit tests. The hooks crate
    /// already depends on tokio (the executor uses `tokio::sync::RwLock`), so
    /// the test runtime can use `tokio::spawn` / `tokio::time::sleep` directly
    /// without pulling in the test-harness mock.
    struct TestRuntime {
        next_id: AtomicU64,
        handles: StdMutex<HashMap<u64, tokio::task::JoinHandle<()>>>,
    }

    impl TestRuntime {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                next_id: AtomicU64::new(1),
                handles: StdMutex::new(HashMap::new()),
            })
        }

        async fn join(&self, handle: &BackgroundTaskHandle) {
            let task = self.handles.lock().unwrap().remove(&handle.task_id);
            if let Some(task) = task {
                let _ = task.await;
            }
        }
    }

    #[async_trait]
    impl RuntimeSpawner for TestRuntime {
        async fn spawn(
            &self,
            name: &str,
            task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
        ) -> Result<BackgroundTaskHandle, RuntimeError> {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let h = tokio::spawn(task);
            self.handles.lock().unwrap().insert(id, h);
            Ok(BackgroundTaskHandle {
                task_name: name.into(),
                task_id: id,
            })
        }
        async fn sleep(&self, duration: Duration) {
            tokio::time::sleep(duration).await;
        }
        async fn cancel(&self, handle: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
            let h = self.handles.lock().unwrap().remove(&handle.task_id);
            if let Some(h) = h {
                h.abort();
            }
            Ok(())
        }
    }

    struct TestPublicationGuard {
        root: lingxi_core::host::CancellationToken,
        publication_lock: Arc<tokio::sync::Mutex<()>>,
        publish_entered: Option<Arc<Notify>>,
    }

    impl HookPublicationGuard for TestPublicationGuard {
        fn is_current(&self) -> bool {
            !self.root.is_cancelled()
        }

        fn generation_cancellation_token(&self) -> Option<lingxi_core::host::CancellationToken> {
            Some(self.root.clone())
        }

        fn publish_if_current<'a>(
            &'a self,
            publication: Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
        ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
            let root = self.root.clone();
            let lock = Arc::clone(&self.publication_lock);
            let publish_entered = self.publish_entered.clone();
            Box::pin(async move {
                let _lease = tokio::select! {
                    biased;
                    () = root.cancelled() => return false,
                    lease = lock.lock_owned() => lease,
                };
                if root.is_cancelled() {
                    return false;
                }
                let publication = async move {
                    if let Some(entered) = publish_entered {
                        entered.notify_one();
                    }
                    publication.await;
                };
                tokio::select! {
                    biased;
                    () = root.cancelled() => false,
                    () = publication => true,
                }
            })
        }

        fn commit_if_current<'a>(
            &'a self,
            mutation: Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
        ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
            let root = self.root.clone();
            let lock = Arc::clone(&self.publication_lock);
            Box::pin(async move {
                let _lease = tokio::select! {
                    biased;
                    () = root.cancelled() => return false,
                    lease = lock.lock_owned() => lease,
                };
                if root.is_cancelled() {
                    return false;
                }
                mutation.await;
                true
            })
        }
    }

    fn ok_result(stdout: &str) -> HookResult {
        HookResult {
            outcome: HookOutcome::Success,
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: Some(0),
            response: None,
        }
    }

    #[tokio::test]
    async fn spawn_records_in_flight_then_publishes_result() {
        let runtime = TestRuntime::new();
        let (tx, mut rx) = mpsc::channel(4);
        let reg = AsyncHookRegistry::new(runtime, tx);

        let hook_id = HookId::new();
        // A hook whose completion we gate on a Notify so we can observe the
        // in-flight entry BEFORE it resolves.
        let gate = Arc::new(Notify::new());
        let gate2 = gate.clone();
        let work: HookWork = Box::pin(async move {
            gate2.notified().await;
            ok_result("done")
        });

        let handle = reg
            .spawn(hook_id, Some(Duration::from_secs(30)), work, None)
            .await
            .expect("spawn must succeed");
        assert_eq!(handle.task_name, "async_hook");

        // The hook is parked on the gate → still in-flight, nothing published.
        assert!(reg.is_in_flight(hook_id).await, "must be tracked in-flight");
        assert_eq!(reg.in_flight_len().await, 1);

        // Release the hook; its result must arrive on completion_tx and the
        // in-flight entry must clear.
        gate.notify_one();
        let HookCompletionEnvelope {
            hook_id: got_id,
            result: got,
            ..
        } = rx.recv().await.expect("completion must publish");
        assert_eq!(got_id, hook_id);
        assert!(matches!(got.outcome, HookOutcome::Success));
        assert_eq!(got.stdout, "done");

        // Drain the in-flight removal (it happens just before the send, but the
        // map lock may settle a beat later — poll briefly).
        for _ in 0..50 {
            if !reg.is_in_flight(hook_id).await {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(
            !reg.is_in_flight(hook_id).await,
            "in-flight entry must clear on completion"
        );
    }

    #[tokio::test]
    async fn obsolete_background_hook_completion_never_reenters_the_session() {
        let runtime = TestRuntime::new();
        let (tx, mut rx) = mpsc::channel(2);
        let reg = AsyncHookRegistry::new(runtime.clone(), tx);
        let root = lingxi_core::host::CancellationToken::new();
        let guard: Arc<dyn HookPublicationGuard> = Arc::new(TestPublicationGuard {
            root: root.clone(),
            publication_lock: Arc::new(tokio::sync::Mutex::new(())),
            publish_entered: None,
        });
        let finalized = Arc::new(AtomicBool::new(false));
        let finalized_for_callback = Arc::clone(&finalized);
        let completion: HookCompletion = Box::new(move |result| {
            Box::pin(async move {
                finalized_for_callback.store(true, Ordering::SeqCst);
                result
            })
        });
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (work_finished_tx, work_finished_rx) = tokio::sync::oneshot::channel();
        let work: HookWork = Box::pin(async move {
            release_rx.await.expect("test releases late work");
            let _ = work_finished_tx.send(());
            ok_result("late result")
        });
        let hook_id = HookId::new();
        let handle = reg
            .spawn_with_completion_and_rewake_for_event(
                hook_id,
                Some(Duration::from_secs(30)),
                work,
                Some(completion),
                None,
                Some("PostToolUse".into()),
                Some(guard),
            )
            .await
            .expect("spawn");

        assert!(reg.is_in_flight(hook_id).await);
        root.cancel();
        release_tx.send(()).expect("release work");
        work_finished_rx
            .await
            .expect("work reached its late completion");
        runtime.join(&handle).await;

        assert!(
            !finalized.load(Ordering::SeqCst),
            "an obsolete result must not run its external finalizer"
        );
        assert!(
            rx.try_recv().is_err(),
            "an obsolete async hook result must not re-enter the session channel"
        );
        assert_eq!(reg.in_flight_len().await, 0);
    }

    #[tokio::test]
    async fn reset_cancels_backpressured_completion_send_after_durable_finalize() {
        let runtime = TestRuntime::new();
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(HookCompletionEnvelope {
            hook_id: HookId::new(),
            result: ok_result("occupies the channel"),
            hook_event: None,
            publication_guard: None,
        })
        .expect("fill the one-slot completion channel");
        let reg = AsyncHookRegistry::new(runtime.clone(), tx);
        let root = lingxi_core::host::CancellationToken::new();
        let publication_lock = Arc::new(tokio::sync::Mutex::new(()));
        let send_started = Arc::new(Notify::new());
        let guard: Arc<dyn HookPublicationGuard> = Arc::new(TestPublicationGuard {
            root: root.clone(),
            publication_lock: Arc::clone(&publication_lock),
            publish_entered: Some(Arc::clone(&send_started)),
        });
        let (finalized_tx, finalized_rx) = tokio::sync::oneshot::channel();
        let completion: HookCompletion = Box::new(move |result| {
            Box::pin(async move {
                let _ = finalized_tx.send(());
                result
            })
        });
        let hook_id = HookId::new();
        let handle = reg
            .spawn_with_completion_and_rewake_for_event(
                hook_id,
                Some(Duration::from_secs(30)),
                Box::pin(async { ok_result("persisted before blocked send") }),
                Some(completion),
                None,
                Some("PostToolUse".into()),
                Some(guard),
            )
            .await
            .expect("spawn");

        finalized_rx
            .await
            .expect("durable finalizer completed before the blocked channel send");
        send_started.notified().await;
        root.cancel();
        runtime.join(&handle).await;

        assert_eq!(rx.try_recv().unwrap().result.stdout, "occupies the channel");
        assert!(
            rx.try_recv().is_err(),
            "stale completion send was cancelled"
        );
        assert!(
            publication_lock.clone().try_lock_owned().is_ok(),
            "reset must not remain blocked behind completion-channel backpressure"
        );
    }

    #[tokio::test]
    async fn spawn_timeout_publishes_timeout_outcome() {
        let runtime = TestRuntime::new();
        let (tx, mut rx) = mpsc::channel(4);
        let reg = AsyncHookRegistry::new(runtime, tx);
        let finalized = Arc::new(StdMutex::new(Vec::new()));
        let finalized_for_callback = finalized.clone();

        let hook_id = HookId::new();
        // A hook that never completes within the timeout window.
        let work: HookWork = Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            ok_result("never")
        });

        let completion: HookCompletion = Box::new(move |result| {
            Box::pin(async move {
                finalized_for_callback.lock().unwrap().push(result.outcome);
                result
            })
        });
        reg.spawn_with_completion(
            hook_id,
            Some(Duration::from_millis(10)),
            work,
            Some(completion),
            None,
        )
        .await
        .expect("spawn must succeed");

        let HookCompletionEnvelope {
            hook_id: got_id,
            result: got,
            ..
        } = rx.recv().await.expect("timeout must publish a result");
        assert_eq!(got_id, hook_id);
        assert!(
            matches!(got.outcome, HookOutcome::Timeout),
            "timeout must carry HookOutcome::Timeout"
        );
        assert!(got.stderr.contains("timed out"));
        assert_eq!(
            finalized.lock().unwrap().as_slice(),
            [HookOutcome::Timeout],
            "the finalizer observes registry-generated timeouts"
        );
    }

    #[tokio::test]
    async fn spawn_defaults_to_15s_timeout_when_none() {
        // With a None timeout the default is 15s, so a fast hook still wins the
        // race and publishes its own result (not a timeout).
        let runtime = TestRuntime::new();
        let (tx, mut rx) = mpsc::channel(4);
        let reg = AsyncHookRegistry::new(runtime, tx);

        let hook_id = HookId::new();
        let work: HookWork = Box::pin(async move { ok_result("fast") });

        reg.spawn(hook_id, None, work, None).await.expect("spawn");

        let HookCompletionEnvelope { result: got, .. } = rx.recv().await.expect("result");
        assert!(matches!(got.outcome, HookOutcome::Success));
        assert_eq!(got.stdout, "fast");
    }

    #[tokio::test]
    async fn rewake_marks_success_and_preserves_existing_context() {
        let runtime = TestRuntime::new();
        let (tx, mut rx) = mpsc::channel(4);
        let reg = AsyncHookRegistry::new(runtime, tx);

        let mut result = ok_result("done");
        result.response = Some(crate::response::HookResponse {
            additional_context: Some("hook output".into()),
            ..Default::default()
        });
        let work: HookWork = Box::pin(async move { result });

        reg.spawn_with_completion_and_rewake(
            HookId::new(),
            Some(Duration::from_secs(1)),
            work,
            None,
            Some("continue now".into()),
            None,
        )
        .await
        .expect("spawn");

        let HookCompletionEnvelope { result: got, .. } = rx.recv().await.expect("completion");
        let response = got.response.expect("rewake response");
        assert!(response.async_rewake);
        assert_eq!(
            response
                .additional_context
                .as_ref()
                .map(|text| text.display.as_str()),
            Some("hook output\ncontinue now")
        );
    }

    #[tokio::test]
    async fn rewake_timeout_still_publishes_bounded_context() {
        let runtime = TestRuntime::new();
        let (tx, mut rx) = mpsc::channel(4);
        let reg = AsyncHookRegistry::new(runtime, tx);
        let work: HookWork = Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            ok_result("never")
        });
        let oversized = "x".repeat(MAX_REWAKE_MESSAGE_CHARS + 25);

        reg.spawn_with_completion_and_rewake(
            HookId::new(),
            Some(Duration::from_millis(10)),
            work,
            None,
            Some(oversized),
            None,
        )
        .await
        .expect("spawn");

        let HookCompletionEnvelope { result: got, .. } =
            rx.recv().await.expect("timeout completion");
        assert!(matches!(got.outcome, HookOutcome::Timeout));
        let response = got.response.expect("timeout rewake response");
        assert!(response.async_rewake);
        assert_eq!(
            response
                .additional_context
                .as_ref()
                .expect("bounded message")
                .len_utf16(),
            MAX_REWAKE_MESSAGE_CHARS
        );
    }
}
