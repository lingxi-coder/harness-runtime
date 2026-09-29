#![allow(clippy::unwrap_used)]

use super::*;
use platform_api::filesystem::{FileContent, FileEvent, FileSystem, FlockGuard, FsError};
use platform_api::tool_invoker::{SubagentInvocationContext, ToolInvokerError};
use platform_api::{
    BudgetError, FusionOrigin, FusionPreset, FusionRequest, FusionRunOutcome, FusionRunRecorder,
    FusionSlashPublicationTarget, FusionTiming, FusionUsage,
};
use serde_json::json;
use std::any::Any;
use std::collections::HashMap as StdHashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;
use tempfile::tempdir;
use test_harness::mocks::MockRuntimeSpawner;
use tokio::sync::{oneshot, Mutex as TokioMutex};

const TEST_CONVERSATION_ID: &str = "sess:11111111-2222-4333-8444-555555555555";

struct MockInvoker;

#[async_trait]
impl ToolInvoker for MockInvoker {
    async fn invoke(
        &self,
        _name: &str,
        _input: serde_json::Value,
        _ctx: SubagentInvocationContext,
    ) -> Result<serde_json::Value, ToolInvokerError> {
        Ok(json!(null))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct MockBudget;

#[async_trait]
impl BudgetEnforcerHandle for MockBudget {
    async fn check_and_charge(&self, _nano_usd: u64) -> Result<(), BudgetError> {
        Ok(())
    }

    async fn snapshot_total_nano_usd(&self) -> u64 {
        0
    }
}

struct InMemoryFs {
    files: TokioMutex<StdHashMap<String, String>>,
}

impl InMemoryFs {
    fn new() -> Self {
        Self {
            files: TokioMutex::new(StdHashMap::new()),
        }
    }
}

#[async_trait]
impl FileSystem for InMemoryFs {
    async fn read_file(
        &self,
        path: &str,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<FileContent, FsError> {
        let map = self.files.lock().await;
        let content = map.get(path).cloned().unwrap_or_default();
        let off = usize::try_from(offset.unwrap_or(0)).unwrap_or(usize::MAX);
        let body: String = content.chars().skip(off).collect();
        let truncated = limit.is_some_and(|lim| body.len() as u64 > lim);
        let trimmed = match limit {
            Some(lim) => body
                .chars()
                .take(usize::try_from(lim).unwrap_or(usize::MAX))
                .collect(),
            None => body,
        };
        Ok(FileContent {
            total_lines: content.lines().count() as u64,
            content: trimmed,
            truncated,
        })
    }

    async fn write_file(&self, path: &str, body: &str) -> Result<(), FsError> {
        self.files
            .lock()
            .await
            .insert(path.to_string(), body.to_string());
        Ok(())
    }

    fn is_within_workspace(&self, _path: &str) -> bool {
        true
    }

    async fn watch(
        &self,
        _path: &str,
    ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = FileEvent> + Send>>, FsError> {
        Err(FsError::Io("not supported".into()))
    }

    async fn append_file(&self, path: &str, body: &str) -> Result<(), FsError> {
        let mut map = self.files.lock().await;
        map.entry(path.to_string()).or_default().push_str(body);
        Ok(())
    }

    async fn truncate(&self, path: &str, len: u64) -> Result<(), FsError> {
        let mut map = self.files.lock().await;
        if let Some(content) = map.get_mut(path) {
            content.truncate(usize::try_from(len).unwrap_or(usize::MAX));
        }
        Ok(())
    }

    async fn file_mtime(&self, _path: &str) -> Result<std::time::SystemTime, FsError> {
        Ok(std::time::SystemTime::UNIX_EPOCH)
    }

    async fn file_size(&self, path: &str) -> Result<u64, FsError> {
        let map = self.files.lock().await;
        Ok(map.get(path).map_or(0, |s| s.len() as u64))
    }

    async fn delete_file(&self, path: &str) -> Result<(), FsError> {
        self.files.lock().await.remove(path);
        Ok(())
    }

    async fn symlink(&self, _src: &str, _dst: &str) -> Result<(), FsError> {
        Ok(())
    }

    async fn flock_exclusive(&self, _path: &str) -> Result<Box<dyn FlockGuard>, FsError> {
        Err(FsError::Io("not supported".into()))
    }

    async fn fsync(&self, _path: &str) -> Result<(), FsError> {
        Ok(())
    }
}

/// An executor that blocks on the inherited cancel token instead of resolving
/// immediately — lets a test observe whether `kill` gives the run a real
/// chance to unwind cooperatively (F012) or hard-aborts it mid-flight.
struct CancelAwareExecutor {
    runs: AtomicUsize,
    observed_cancel: AtomicUsize,
}

impl CancelAwareExecutor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            runs: AtomicUsize::new(0),
            observed_cancel: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl FusionExecutor for CancelAwareExecutor {
    fn prepare(
        self: ::std::sync::Arc<Self>,
        submission: ::platform_api::FusionSubmission,
    ) -> Result<::platform_api::PreparedFusionRun, ::platform_api::FusionError> {
        let this = ::std::sync::Arc::clone(&self);
        let timeout = self.effective_timeout_ms();
        ::platform_api::prepared_from_oneshot(
            submission,
            timeout,
            move |_request, inherit, _progress| async move {
                this.runs.fetch_add(1, Ordering::SeqCst);
                inherit.cancel.cancelled().await;
                this.observed_cancel.fetch_add(1, Ordering::SeqCst);
                Err(FusionError::Cancelled)
            },
        )
    }
}

struct ImmediateExecutor {
    runs: AtomicUsize,
}

impl ImmediateExecutor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            runs: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl FusionExecutor for ImmediateExecutor {
    fn prepare(
        self: ::std::sync::Arc<Self>,
        submission: ::platform_api::FusionSubmission,
    ) -> Result<::platform_api::PreparedFusionRun, ::platform_api::FusionError> {
        let this = ::std::sync::Arc::clone(&self);
        let timeout = self.effective_timeout_ms();
        ::platform_api::prepared_from_oneshot(
            submission,
            timeout,
            move |_request, _inherit, _progress| async move {
                this.runs.fetch_add(1, Ordering::SeqCst);
                Ok(dummy_result())
            },
        )
    }
}

/// Captures the timeout that the handler carries into the actual Fusion run.
/// This guards the per-run snapshot seam separately from the print-mode
/// deadline projection.
struct TimeoutSnapshotExecutor {
    observed: StdMutex<Vec<Option<u64>>>,
}

impl TimeoutSnapshotExecutor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            observed: StdMutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl FusionExecutor for TimeoutSnapshotExecutor {
    fn prepare(
        self: ::std::sync::Arc<Self>,
        submission: ::platform_api::FusionSubmission,
    ) -> Result<::platform_api::PreparedFusionRun, ::platform_api::FusionError> {
        let this = ::std::sync::Arc::clone(&self);
        let timeout = self.effective_timeout_ms();
        ::platform_api::prepared_from_oneshot(
            submission,
            timeout,
            move |_request, inherit, _progress| async move {
                this.observed
                    .lock()
                    .unwrap()
                    .push(inherit.effective_timeout_ms);
                Ok(dummy_result())
            },
        )
    }

    fn effective_timeout_ms(&self) -> Option<u64> {
        Some(3_600_000)
    }
}

/// F005: sends two scripted `FusionProgress` events through whatever channel
/// `spawn` hands it before completing — the fixture under test is the
/// HANDLER's forwarding wire, not the orchestrator.
struct ProgressEmittingExecutor {
    runs: AtomicUsize,
}

impl ProgressEmittingExecutor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            runs: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl FusionExecutor for ProgressEmittingExecutor {
    fn prepare(
        self: ::std::sync::Arc<Self>,
        submission: ::platform_api::FusionSubmission,
    ) -> Result<::platform_api::PreparedFusionRun, ::platform_api::FusionError> {
        let this = ::std::sync::Arc::clone(&self);
        let timeout = self.effective_timeout_ms();
        ::platform_api::prepared_from_oneshot(
            submission,
            timeout,
            move |_request, _inherit, progress| async move {
                this.runs.fetch_add(1, Ordering::SeqCst);
                if let Some(tx) = progress {
                    for stage in [
                        platform_api::FusionStage::ResolvingModels,
                        platform_api::FusionStage::RunningPanels {
                            completed: 2,
                            total: 3,
                        },
                    ] {
                        let _ = tx
                            .send(platform_api::FusionProgress {
                                message: stage.label(),
                                stage,
                                panel_id: None,
                                realized_output_tokens: None,
                                egress_profiles: None,
                                panels_allocated: None,
                            })
                            .await;
                    }
                }
                Ok(dummy_result())
            },
        )
    }
}

/// Blocks in `run()` until the fusion inheritance's cancel token fires (the
/// same pattern `local_workflow_test::BlockingFusionExecutor` uses), then
/// returns `Err(Cancelled)`. Signals `started` once it has actually entered
/// `run()`, so a test can wait for the worker to be genuinely in-flight
/// before calling `kill`.
struct BlockingCancelAwareExecutor {
    started: StdMutex<Option<oneshot::Sender<()>>>,
    runs: AtomicUsize,
}

impl BlockingCancelAwareExecutor {
    fn new(started: oneshot::Sender<()>) -> Arc<Self> {
        Arc::new(Self {
            started: StdMutex::new(Some(started)),
            runs: AtomicUsize::new(0),
        })
    }
}

/// Natural result fixture used to prove the handler leaves a worker in place
/// when the shared control has already crossed into Finalizing. The runner
/// deliberately ignores cancellation so the test can release the natural
/// result after kill/drain loses the atomic request_cancel arbitration.
struct NaturalAfterFinalizingExecutor {
    started: StdMutex<Option<oneshot::Sender<()>>>,
    release: TokioMutex<Option<oneshot::Receiver<()>>>,
}

impl NaturalAfterFinalizingExecutor {
    fn new(started: oneshot::Sender<()>, release: oneshot::Receiver<()>) -> Arc<Self> {
        Arc::new(Self {
            started: StdMutex::new(Some(started)),
            release: TokioMutex::new(Some(release)),
        })
    }
}

#[async_trait]
impl FusionExecutor for NaturalAfterFinalizingExecutor {
    fn prepare(
        self: ::std::sync::Arc<Self>,
        submission: ::platform_api::FusionSubmission,
    ) -> Result<::platform_api::PreparedFusionRun, ::platform_api::FusionError> {
        let this = ::std::sync::Arc::clone(&self);
        let timeout = self.effective_timeout_ms();
        ::platform_api::prepared_from_oneshot(
            submission,
            timeout,
            move |_request, _inherit, _progress| async move {
                if let Some(tx) = this.started.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                if let Some(release) = this.release.lock().await.take() {
                    let _ = release.await;
                }
                Ok(dummy_result())
            },
        )
    }
}

#[async_trait]
impl FusionExecutor for BlockingCancelAwareExecutor {
    fn prepare(
        self: ::std::sync::Arc<Self>,
        submission: ::platform_api::FusionSubmission,
    ) -> Result<::platform_api::PreparedFusionRun, ::platform_api::FusionError> {
        let this = ::std::sync::Arc::clone(&self);
        let timeout = self.effective_timeout_ms();
        ::platform_api::prepared_from_oneshot(
            submission,
            timeout,
            move |_request, inherit, _progress| async move {
                this.runs.fetch_add(1, Ordering::SeqCst);
                if let Some(tx) = this.started.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                inherit.cancel.cancelled().await;
                Err(FusionError::Cancelled)
            },
        )
    }
}

/// [Round-4 review finding 10] Sends ONE progress event carrying real
/// `realized_output_tokens`/`egress_profiles` (the shape `run()`'s outer
/// cancel/timeout `Err` arm emits after real panel spend — see
/// `fusion::orchestrator::run`), signals `started` once that event is
/// sent, then blocks on the inherited cancel token before returning
/// `Err(Cancelled)` — the same production shape `BlockingCancelAwareExecutor`
/// models, plus the progress payload a real cancelled run would have
/// already reported before `kill` ever removes the worker record.
struct RealizedProgressBlockingExecutor {
    started: StdMutex<Option<oneshot::Sender<()>>>,
    runs: AtomicUsize,
}

impl RealizedProgressBlockingExecutor {
    fn new(started: oneshot::Sender<()>) -> Arc<Self> {
        Arc::new(Self {
            started: StdMutex::new(Some(started)),
            runs: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl FusionExecutor for RealizedProgressBlockingExecutor {
    fn prepare(
        self: ::std::sync::Arc<Self>,
        submission: ::platform_api::FusionSubmission,
    ) -> Result<::platform_api::PreparedFusionRun, ::platform_api::FusionError> {
        let this = ::std::sync::Arc::clone(&self);
        let timeout = self.effective_timeout_ms();
        ::platform_api::prepared_from_oneshot(
            submission,
            timeout,
            move |_request, inherit, progress| async move {
                this.runs.fetch_add(1, Ordering::SeqCst);
                if let Some(tx) = &progress {
                    let _ = tx
                        .send(platform_api::FusionProgress {
                            message: "panels billed real usage".into(),
                            stage: platform_api::FusionStage::Analyzing,
                            panel_id: None,
                            realized_output_tokens: Some(12),
                            egress_profiles: Some(vec!["anthropic".into(), "openai".into()]),
                            panels_allocated: None,
                        })
                        .await;
                }
                if let Some(tx) = this.started.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                inherit.cancel.cancelled().await;
                Err(FusionError::Cancelled)
            },
        )
    }
}

#[derive(Default)]
struct CountingCompletionSink(AtomicUsize);

#[async_trait]
impl FusionCompletionSink for CountingCompletionSink {
    async fn publish(
        &self,
        _conversation_id: &str,
        _result: &FusionResult,
    ) -> platform_api::FusionPublicationReceipt {
        self.0.fetch_add(1, Ordering::SeqCst);
        platform_api::FusionPublicationReceipt::published()
    }
}

/// Production-shaped terminal capability used to prove that the prepared
/// supervisor records before the handler exposes a completed task and never
/// invokes the legacy completion sink as a second publication path.
struct RecordingTerminalRecorder {
    calls: AtomicUsize,
    publication: platform_api::FusionPublicationReceipt,
}

#[async_trait]
impl FusionRunRecorder for RecordingTerminalRecorder {
    async fn record_terminal(
        &self,
        _outcome: FusionRunOutcome,
        _slash_target: Option<FusionSlashPublicationTarget>,
    ) -> platform_api::FusionPublicationReceipt {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.publication.clone()
    }
}

#[derive(Default)]
struct ActivationSink {
    statuses: StdMutex<Vec<(String, TaskStatus)>>,
    terminal: StdMutex<Vec<(String, String, String, TaskStatus)>>,
}

#[async_trait]
impl TaskStatusSink for ActivationSink {
    fn requires_explicit_activation(&self) -> bool {
        true
    }

    async fn set_status(&self, task_id: &str, status: TaskStatus) {
        self.statuses
            .lock()
            .unwrap()
            .push((task_id.to_string(), status));
    }

    async fn finish_fusion_terminal(
        &self,
        task_id: &str,
        run_id: String,
        final_text: String,
        status: TaskStatus,
    ) {
        self.terminal
            .lock()
            .unwrap()
            .push((task_id.to_string(), run_id, final_text, status));
        self.set_status(task_id, status).await;
    }

    async fn is_terminal(&self, task_id: &str) -> bool {
        self.statuses
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(id, _)| id == task_id)
            .is_some_and(|(_, status)| status.is_terminal())
    }
}

impl ActivationSink {
    fn last_status(&self) -> Option<TaskStatus> {
        self.statuses
            .lock()
            .unwrap()
            .last()
            .map(|(_, status)| *status)
    }

    fn statuses(&self) -> Vec<TaskStatus> {
        self.statuses
            .lock()
            .unwrap()
            .iter()
            .map(|(_, status)| *status)
            .collect()
    }
}

#[derive(Default)]
struct RecordingSink {
    statuses: StdMutex<Vec<(String, TaskStatus)>>,
    calls: StdMutex<Vec<&'static str>>,
    /// F005: every `set_fusion_stage` label, in call order.
    stages: StdMutex<Vec<String>>,
}

impl RecordingSink {
    fn last_status(&self) -> Option<TaskStatus> {
        self.statuses
            .lock()
            .unwrap()
            .last()
            .map(|(_, status)| *status)
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }

    fn count(&self, status: TaskStatus) -> usize {
        self.statuses
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, s)| *s == status)
            .count()
    }

    fn stages(&self) -> Vec<String> {
        self.stages.lock().unwrap().clone()
    }
}

#[async_trait]
impl TaskStatusSink for RecordingSink {
    async fn set_status(&self, task_id: &str, status: TaskStatus) {
        self.calls.lock().unwrap().push("status");
        self.statuses
            .lock()
            .unwrap()
            .push((task_id.to_string(), status));
    }

    async fn set_fusion_stage(&self, _task_id: &str, stage: String) {
        self.stages.lock().unwrap().push(stage);
    }

    async fn is_terminal(&self, task_id: &str) -> bool {
        self.statuses
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(id, _)| id == task_id)
            .is_some_and(|(_, status)| status.is_terminal())
    }
}

struct BlockingFusionTerminalSink {
    inner: RecordingSink,
    started_tx: StdMutex<Option<oneshot::Sender<()>>>,
    release_rx: TokioMutex<Option<oneshot::Receiver<()>>>,
}

impl BlockingFusionTerminalSink {
    fn new(started_tx: oneshot::Sender<()>, release_rx: oneshot::Receiver<()>) -> Self {
        Self {
            inner: RecordingSink::default(),
            started_tx: StdMutex::new(Some(started_tx)),
            release_rx: TokioMutex::new(Some(release_rx)),
        }
    }

    fn last_status(&self) -> Option<TaskStatus> {
        self.inner.last_status()
    }

    fn calls(&self) -> Vec<&'static str> {
        self.inner.calls()
    }
}

#[async_trait]
impl TaskStatusSink for BlockingFusionTerminalSink {
    async fn set_status(&self, task_id: &str, status: TaskStatus) {
        self.inner.calls.lock().unwrap().push("status");
        self.inner
            .statuses
            .lock()
            .unwrap()
            .push((task_id.to_string(), status));
    }

    async fn finish_fusion_terminal(
        &self,
        task_id: &str,
        _run_id: String,
        _final_text: String,
        status: TaskStatus,
    ) {
        self.inner.calls.lock().unwrap().push("outcome");
        if let Some(tx) = self.started_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
        if let Some(rx) = self.release_rx.lock().await.take() {
            let _ = rx.await;
        }
        self.set_status(task_id, status).await;
    }

    async fn is_terminal(&self, task_id: &str) -> bool {
        self.inner
            .statuses
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(id, _)| id == task_id)
            .is_some_and(|(_, status)| status.is_terminal())
    }
}

struct FailingExecutor {
    error: FusionError,
}

#[async_trait]
impl FusionExecutor for FailingExecutor {
    fn prepare(
        self: ::std::sync::Arc<Self>,
        submission: ::platform_api::FusionSubmission,
    ) -> Result<::platform_api::PreparedFusionRun, ::platform_api::FusionError> {
        let this = ::std::sync::Arc::clone(&self);
        let timeout = self.effective_timeout_ms();
        ::platform_api::prepared_from_oneshot(
            submission,
            timeout,
            move |_request, _inherit, _progress| async move { Err(this.error.clone()) },
        )
    }
}

/// Records every [`TaskStatusSink`] call in order (event tag first, so a
/// test can assert both the exact sequence and the payload of each call
/// rather than only that *some* status/outcome eventually landed).
#[derive(Default)]
struct RecordingStatusSink {
    events: Arc<StdMutex<Vec<String>>>,
    statuses: StdMutex<Vec<(String, TaskStatus)>>,
    errors: StdMutex<Vec<(String, String)>>,
    egress_and_usage: StdMutex<
        Vec<(
            String,
            Vec<String>,
            Option<platform_api::task_registry::AgentRunUsage>,
        )>,
    >,
}

impl RecordingStatusSink {
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    /// Shares this sink's event log with a [`PublishOrderCompletionSink`] so
    /// a test can assert `publish` and `mark_fusion_result_published` land
    /// in the SAME ordered timeline as `finish_fusion_terminal` / the other
    /// status-sink calls, not just that both eventually happened.
    fn events_handle(&self) -> Arc<StdMutex<Vec<String>>> {
        self.events.clone()
    }

    fn last_status(&self) -> Option<TaskStatus> {
        self.statuses.lock().unwrap().last().map(|(_, s)| *s)
    }

    fn errors(&self) -> Vec<(String, String)> {
        self.errors.lock().unwrap().clone()
    }

    fn egress_and_usage(
        &self,
    ) -> Vec<(
        String,
        Vec<String>,
        Option<platform_api::task_registry::AgentRunUsage>,
    )> {
        self.egress_and_usage.lock().unwrap().clone()
    }
}

#[async_trait]
impl TaskStatusSink for RecordingStatusSink {
    async fn set_status(&self, task_id: &str, status: TaskStatus) {
        self.events
            .lock()
            .unwrap()
            .push(format!("status:{status:?}"));
        self.statuses
            .lock()
            .unwrap()
            .push((task_id.to_string(), status));
    }

    async fn set_fusion_error(&self, task_id: &str, error: String) {
        self.events.lock().unwrap().push(format!("error:{error}"));
        self.errors
            .lock()
            .unwrap()
            .push((task_id.to_string(), error));
    }

    async fn set_fusion_egress_and_usage(
        &self,
        task_id: &str,
        egress_profiles: Vec<String>,
        usage: Option<platform_api::task_registry::AgentRunUsage>,
    ) {
        self.events.lock().unwrap().push(format!(
            "egress_and_usage:{}:{}",
            egress_profiles.len(),
            usage.is_some()
        ));
        self.egress_and_usage
            .lock()
            .unwrap()
            .push((task_id.to_string(), egress_profiles, usage));
    }

    async fn finish_fusion_terminal(
        &self,
        task_id: &str,
        _run_id: String,
        _final_text: String,
        status: TaskStatus,
    ) {
        self.events.lock().unwrap().push("outcome".to_string());
        self.set_status(task_id, status).await;
    }

    async fn set_fusion_publication(
        &self,
        _task_id: &str,
        receipt: platform_api::FusionPublicationReceipt,
    ) {
        self.events
            .lock()
            .unwrap()
            .push(format!("receipt:{:?}", receipt.status));
    }

    async fn is_terminal(&self, task_id: &str) -> bool {
        self.statuses
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(id, _)| id == task_id)
            .is_some_and(|(_, status)| status.is_terminal())
    }
}

/// Pushes `"publish"` into a [`RecordingStatusSink`]'s shared event log
/// (via [`RecordingStatusSink::events_handle`]) so a test can assert the
/// exact interleaving of `FusionCompletionSink::publish` against
/// `TaskStatusSink` calls on the SAME timeline, not two separately-ordered
/// logs a test would have to correlate by hand.
struct PublishOrderCompletionSink(Arc<StdMutex<Vec<String>>>);

#[async_trait]
impl FusionCompletionSink for PublishOrderCompletionSink {
    async fn publish(
        &self,
        _conversation_id: &str,
        _result: &FusionResult,
    ) -> platform_api::FusionPublicationReceipt {
        self.0.lock().unwrap().push("publish".to_string());
        platform_api::FusionPublicationReceipt::published()
    }
}

/// Sends exactly one progress event, then completes successfully. The lone
/// buffered event is what makes `run_fusion_worker`'s progress forwarder
/// (F005) actually do something on `forwarder.await` — required to land
/// [Finding 18]'s window.
struct SingleProgressExecutor {
    runs: AtomicUsize,
}

impl SingleProgressExecutor {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            runs: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl FusionExecutor for SingleProgressExecutor {
    fn prepare(
        self: ::std::sync::Arc<Self>,
        submission: ::platform_api::FusionSubmission,
    ) -> Result<::platform_api::PreparedFusionRun, ::platform_api::FusionError> {
        let this = ::std::sync::Arc::clone(&self);
        let timeout = self.effective_timeout_ms();
        ::platform_api::prepared_from_oneshot(
            submission,
            timeout,
            move |_request, _inherit, progress| async move {
                this.runs.fetch_add(1, Ordering::SeqCst);
                if let Some(tx) = progress {
                    let stage = platform_api::FusionStage::ResolvingModels;
                    let _ = tx
                        .send(platform_api::FusionProgress {
                            message: stage.label(),
                            stage,
                            panel_id: None,
                            realized_output_tokens: None,
                            egress_profiles: None,
                            panels_allocated: None,
                        })
                        .await;
                }
                Ok(dummy_result())
            },
        )
    }
}

/// [Finding 14; round-3 review B2 extends this with `egress_profiles`]:
/// emits one progress event carrying `realized_output_tokens: Some(N)` —
/// the shape the orchestrator sends when `check_panel_bar` fails after real
/// panel spend — and, when `egress_profiles` is non-empty, the resolved
/// egress profile list the orchestrator latches at the same seam (see
/// `platform_api::FusionProgress::egress_profiles`) — then fails with the
/// given error. Used to prove the Err arm discloses both the realized
/// tokens AND the egress profiles it already has access to via the
/// progress forwarder.
struct PartialSpendThenFailExecutor {
    error: FusionError,
    realized_output_tokens: u64,
    egress_profiles: Vec<String>,
}

#[async_trait]
impl FusionExecutor for PartialSpendThenFailExecutor {
    fn prepare(
        self: ::std::sync::Arc<Self>,
        submission: ::platform_api::FusionSubmission,
    ) -> Result<::platform_api::PreparedFusionRun, ::platform_api::FusionError> {
        let this = ::std::sync::Arc::clone(&self);
        let timeout = self.effective_timeout_ms();
        ::platform_api::prepared_from_oneshot(
            submission,
            timeout,
            move |_request, _inherit, progress| async move {
                if let Some(tx) = progress {
                    let stage = platform_api::FusionStage::Failed;
                    let egress_profiles = if this.egress_profiles.is_empty() {
                        None
                    } else {
                        Some(this.egress_profiles.clone())
                    };
                    let _ = tx
                        .send(platform_api::FusionProgress {
                            message: stage.label(),
                            stage,
                            panel_id: None,
                            realized_output_tokens: Some(this.realized_output_tokens),
                            egress_profiles,
                            panels_allocated: None,
                        })
                        .await;
                }
                Err(this.error.clone())
            },
        )
    }
}

/// [Finding 18]: blocks the progress forwarder's very first
/// `set_fusion_stage` call (signalling `started` once inside it) until
/// released, so a test can land `kill` precisely inside `run_fusion_worker`'s
/// `forwarder.await` window — after `executor.run` has already returned
/// `Ok(..)`, but (pre-fix) before the `finalizing` claim.
struct BlockingStageSink {
    inner: RecordingStatusSink,
    started_tx: StdMutex<Option<oneshot::Sender<()>>>,
    release_rx: TokioMutex<Option<oneshot::Receiver<()>>>,
}

impl BlockingStageSink {
    fn new(started_tx: oneshot::Sender<()>, release_rx: oneshot::Receiver<()>) -> Self {
        Self {
            inner: RecordingStatusSink::default(),
            started_tx: StdMutex::new(Some(started_tx)),
            release_rx: TokioMutex::new(Some(release_rx)),
        }
    }

    fn last_status(&self) -> Option<TaskStatus> {
        self.inner.last_status()
    }
}

#[async_trait]
impl TaskStatusSink for BlockingStageSink {
    async fn set_status(&self, task_id: &str, status: TaskStatus) {
        self.inner.set_status(task_id, status).await;
    }

    async fn set_fusion_error(&self, task_id: &str, error: String) {
        self.inner.set_fusion_error(task_id, error).await;
    }

    async fn set_fusion_egress_and_usage(
        &self,
        task_id: &str,
        egress_profiles: Vec<String>,
        usage: Option<platform_api::task_registry::AgentRunUsage>,
    ) {
        self.inner
            .set_fusion_egress_and_usage(task_id, egress_profiles, usage)
            .await;
    }

    async fn set_fusion_stage(&self, _task_id: &str, stage: String) {
        self.inner
            .events
            .lock()
            .unwrap()
            .push(format!("stage:{stage}"));
        if let Some(tx) = self.started_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
        if let Some(rx) = self.release_rx.lock().await.take() {
            let _ = rx.await;
        }
    }

    async fn finish_fusion_terminal(
        &self,
        task_id: &str,
        run_id: String,
        final_text: String,
        status: TaskStatus,
    ) {
        self.inner
            .finish_fusion_terminal(task_id, run_id, final_text, status)
            .await;
    }

    async fn set_fusion_publication(
        &self,
        task_id: &str,
        receipt: platform_api::FusionPublicationReceipt,
    ) {
        self.inner.set_fusion_publication(task_id, receipt).await;
    }

    async fn is_terminal(&self, task_id: &str) -> bool {
        self.inner.is_terminal(task_id).await
    }
}

fn dummy_request() -> FusionRequest {
    FusionRequest {
        verify_claims: false,
        schema_version: 1,
        origin: FusionOrigin::Slash,
        prompt: "review this".into(),
        preset: FusionPreset::Quality,
        models: None,
        dimensions: vec!["coverage".into()],
        partial_ok: true,
        max_panel: None,
        cross_provider: true,
        parent_profile: "openai".into(),
        parent_model: "gpt-5.4".into(),
        mode: Default::default(),
        verify_commands: Vec::new(),
    }
}

fn dummy_result() -> FusionResult {
    FusionResult {
        schema_version: 2,
        run_id: "fu_test".into(),
        status: FusionStatus::Unanalyzed,
        analysis_failure: Some("timeout".into()),
        analysis: None,
        responses: vec![],
        panels: vec![],
        usage: FusionUsage::default(),
        timing: FusionTiming::default(),
        egress_profiles: vec![],
        mode: Default::default(),
    }
}

fn make_ctx(fs: Arc<dyn FileSystem>) -> TaskContext {
    TaskContext {
        fs,
        runtime: Arc::new(MockRuntimeSpawner::default()),
    }
}

fn make_output_manager(fs: Arc<dyn FileSystem>) -> (tempfile::TempDir, Arc<TaskOutputManager>) {
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(PathBuf::from(dir.path()), fs));
    (dir, mgr)
}

#[tokio::test]
async fn accounting_failure_retains_answer_but_does_not_report_success() {
    let fs = Arc::new(InMemoryFs::new());
    let (_directory, output) = make_output_manager(fs.clone());
    let path = output.allocate("accounting-failure").await.unwrap();
    let status = Arc::new(RecordingStatusSink::default());
    let status_trait: Arc<dyn TaskStatusSink> = status.clone();
    let completion: Arc<dyn FusionCompletionSink> = Arc::new(CountingCompletionSink::default());
    let control = platform_api::FusionRunControl::new_with_billing_mode(
        FusionRunIdentity::new(FusionRunId::generated(), None, FusionOrigin::Agent, None),
        1_000,
        CancellationToken::new(),
        platform_api::FusionRunFactsRecorder::default(),
        platform_api::ModelAttemptBillingMode::MeteredAttempts,
    );
    control
        .facts()
        .set_attempt_settlement(platform_api::FusionAttemptSettlementStatus::Failed {
            reason: "ledger unavailable".into(),
        });
    let outcome = FusionRunOutcome::from_control(&control, Ok(dummy_result()));
    finalize_fusion_outcome(
        &outcome,
        &output,
        &path,
        "accounting-failure",
        &status_trait,
        &completion,
        TEST_CONVERSATION_ID,
        None,
        None,
    )
    .await;
    assert_eq!(status.last_status(), Some(TaskStatus::Failed));
    assert!(status.errors()[0].1.contains("ledger unavailable"));
    assert!(status.events().contains(&"outcome".to_string()));
    assert!(fs
        .files
        .lock()
        .await
        .values()
        .any(|body| body.contains("\"unanalyzed\"")));
}

fn make_handler(
    executor: Arc<dyn FusionExecutor>,
    output_manager: Arc<TaskOutputManager>,
    status_sink: Arc<dyn TaskStatusSink>,
    completion_sink: Arc<dyn FusionCompletionSink>,
) -> LocalFusionHandler {
    LocalFusionHandler::new(
        executor,
        completion_sink,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        output_manager,
    )
    .with_status_sink(status_sink)
}

#[tokio::test]
async fn handler_waits_for_activation_before_running_executor() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (_dir, output_manager) = make_output_manager(fs.clone());
    let status_sink = Arc::new(ActivationSink::default());
    let completion_sink = Arc::new(CountingCompletionSink::default());
    let executor = ImmediateExecutor::new();
    let handler = make_handler(
        executor.clone(),
        output_manager,
        status_sink.clone(),
        completion_sink.clone(),
    );

    let mut handle = handler
        .spawn(
            TaskSpawnInput::LocalFusion {
                request: dummy_request(),
                conversation_id: TEST_CONVERSATION_ID.into(),
            },
            make_ctx(fs),
        )
        .await
        .expect("spawn succeeds");

    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(status_sink.statuses().is_empty());
    assert_eq!(executor.runs.load(Ordering::SeqCst), 0);

    handle.activate();
    for _ in 0..200 {
        if status_sink
            .last_status()
            .is_some_and(TaskStatus::is_terminal)
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        status_sink.statuses(),
        vec![TaskStatus::Running, TaskStatus::Completed]
    );
    assert_eq!(executor.runs.load(Ordering::SeqCst), 1);
    assert_eq!(completion_sink.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn handler_uses_prepared_timeout_without_a_legacy_caller_override() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (_dir, output_manager) = make_output_manager(fs.clone());
    let status_sink = Arc::new(RecordingSink::default());
    let completion_sink = Arc::new(CountingCompletionSink::default());
    let executor = TimeoutSnapshotExecutor::new();
    let handler = make_handler(
        executor.clone(),
        output_manager,
        status_sink.clone(),
        completion_sink,
    );

    let handle = handler
        .spawn(
            TaskSpawnInput::LocalFusion {
                request: dummy_request(),
                conversation_id: TEST_CONVERSATION_ID.into(),
            },
            make_ctx(fs),
        )
        .await
        .expect("spawn succeeds");
    assert_eq!(
        handle
            .fusion_prepared_summary
            .as_ref()
            .map(|summary| summary.duration_ms),
        Some(3_600_000)
    );

    for _ in 0..200 {
        if status_sink
            .last_status()
            .is_some_and(TaskStatus::is_terminal)
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        *executor.observed.lock().unwrap(),
        vec![None],
        "the legacy inheritance override stays unset; the prepared adapter owns the deadline"
    );
}

/// F005: `LocalFusionHandler::spawn` must forward every `FusionProgress` the
/// executor emits into `TaskStatusSink::set_fusion_stage`, in order — before
/// this fix `spawn` always passed `None` for the progress channel, so a
/// `/fusion` task's stage never updated between `Running` and its terminal
/// status.
#[tokio::test]
async fn spawn_forwards_fusion_progress_into_set_fusion_stage() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (_dir, output_manager) = make_output_manager(fs.clone());
    let status_sink = Arc::new(RecordingSink::default());
    let completion_sink = Arc::new(CountingCompletionSink::default());
    let executor = ProgressEmittingExecutor::new();
    let handler = make_handler(
        executor.clone(),
        output_manager,
        status_sink.clone(),
        completion_sink,
    );

    handler
        .spawn(
            TaskSpawnInput::LocalFusion {
                request: dummy_request(),
                conversation_id: TEST_CONVERSATION_ID.into(),
            },
            make_ctx(fs),
        )
        .await
        .expect("spawn succeeds");

    for _ in 0..200 {
        if status_sink
            .last_status()
            .is_some_and(TaskStatus::is_terminal)
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(executor.runs.load(Ordering::SeqCst), 1);
    assert_eq!(
        status_sink.stages(),
        vec![
            "Resolving models".to_string(),
            "Running panels 2/3".to_string()
        ],
        "expected the 2 scripted FusionProgress events forwarded in order"
    );
}

/// A print-mode waiter that returns on terminal status alone can race the
/// still-in-flight publication append and, on process exit, lose the durable
/// `<fusion-result>` row. The typed receipt is what distinguishes the two:
/// this pins that it is written, and written strictly AFTER the publish, on
/// the same timeline as the terminal-status transition — never before it,
/// since the notification drain still needs `finish_fusion_terminal` first.
#[tokio::test]
async fn the_publication_receipt_lands_after_the_completion_sink_publish() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (_dir, output_manager) = make_output_manager(fs.clone());
    let status_sink = Arc::new(RecordingStatusSink::default());
    let completion_sink = Arc::new(PublishOrderCompletionSink(status_sink.events_handle()));
    let executor = ImmediateExecutor::new();
    let handler = make_handler(
        executor.clone(),
        output_manager,
        status_sink.clone(),
        completion_sink,
    );

    handler
        .spawn(
            TaskSpawnInput::LocalFusion {
                request: dummy_request(),
                conversation_id: TEST_CONVERSATION_ID.into(),
            },
            make_ctx(fs),
        )
        .await
        .expect("spawn succeeds");

    for _ in 0..200 {
        if status_sink.events().contains(&"published".to_string()) {
            break;
        }
        tokio::task::yield_now().await;
    }

    assert_eq!(
        status_sink.events(),
        vec![
            "status:Running".to_string(),
            "egress_and_usage:0:true".to_string(),
            "outcome".to_string(),
            "status:Completed".to_string(),
            "publish".to_string(),
            "receipt:Published".to_string(),
        ],
        "the Published receipt must land, and strictly after both \
         finish_fusion_terminal (\"outcome\"/\"status:Completed\") and the \
         completion sink's own publish — never before either"
    );
}

#[tokio::test]
async fn prepared_terminal_recorder_precedes_task_completion_without_legacy_publish() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (_dir, output_manager) = make_output_manager(fs.clone());
    let status_sink = Arc::new(RecordingStatusSink::default());
    let completion_sink = Arc::new(CountingCompletionSink::default());
    let recorder = Arc::new(RecordingTerminalRecorder {
        calls: AtomicUsize::new(0),
        publication: platform_api::FusionPublicationReceipt::published(),
    });
    let handler = make_handler(
        ImmediateExecutor::new(),
        output_manager,
        status_sink.clone(),
        completion_sink.clone(),
    )
    .with_terminal_recorder(recorder.clone());

    handler
        .spawn(
            TaskSpawnInput::LocalFusion {
                request: dummy_request(),
                conversation_id: TEST_CONVERSATION_ID.into(),
            },
            make_ctx(fs),
        )
        .await
        .expect("spawn succeeds");

    for _ in 0..200 {
        if status_sink.last_status() == Some(TaskStatus::Completed) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(recorder.calls.load(Ordering::SeqCst), 1);
    assert_eq!(completion_sink.0.load(Ordering::SeqCst), 0);
    assert_eq!(status_sink.last_status(), Some(TaskStatus::Completed));
}

#[tokio::test]
async fn kill_preserves_terminalizing_fusion_window() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (_dir, output_manager) = make_output_manager(fs.clone());
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let status_sink = Arc::new(BlockingFusionTerminalSink::new(started_tx, release_rx));
    let completion_sink = Arc::new(CountingCompletionSink::default());
    let handler = make_handler(
        ImmediateExecutor::new(),
        output_manager,
        status_sink.clone(),
        completion_sink,
    );
    let ctx = make_ctx(fs);

    let handle = handler
        .spawn(
            TaskSpawnInput::LocalFusion {
                request: dummy_request(),
                conversation_id: TEST_CONVERSATION_ID.into(),
            },
            ctx.clone(),
        )
        .await
        .expect("spawn succeeds");

    tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
        .await
        .expect("worker should enter terminal publish")
        .expect("terminal publish signal");

    handler
        .kill(&handle.task_id, ctx)
        .await
        .expect("kill should short-circuit during terminal publish");

    let _ = release_tx.send(());
    for _ in 0..200 {
        if status_sink
            .last_status()
            .is_some_and(TaskStatus::is_terminal)
        {
            break;
        }
        tokio::task::yield_now().await;
    }

    assert_eq!(status_sink.last_status(), Some(TaskStatus::Completed));
    assert_eq!(status_sink.calls(), vec!["status", "outcome", "status"]);
}

#[tokio::test]
async fn drain_pending_kills_preserves_terminalizing_fusion_window() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (_dir, output_manager) = make_output_manager(fs.clone());
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let status_sink = Arc::new(BlockingFusionTerminalSink::new(started_tx, release_rx));
    let completion_sink = Arc::new(CountingCompletionSink::default());
    let handler = make_handler(
        ImmediateExecutor::new(),
        output_manager,
        status_sink.clone(),
        completion_sink,
    );

    let handle = handler
        .spawn(
            TaskSpawnInput::LocalFusion {
                request: dummy_request(),
                conversation_id: TEST_CONVERSATION_ID.into(),
            },
            make_ctx(fs),
        )
        .await
        .expect("spawn succeeds");

    tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
        .await
        .expect("worker should enter terminal publish")
        .expect("terminal publish signal");

    (handle.cleanup.as_ref().expect("cleanup seam"))();
    handler.drain_pending_kills().await;

    let _ = release_tx.send(());
    for _ in 0..200 {
        if status_sink
            .last_status()
            .is_some_and(TaskStatus::is_terminal)
        {
            break;
        }
        tokio::task::yield_now().await;
    }

    assert_eq!(status_sink.last_status(), Some(TaskStatus::Completed));
    assert_eq!(status_sink.calls(), vec!["status", "outcome", "status"]);
}

#[tokio::test]
async fn kill_request_cancel_loses_to_an_atomic_finalizing_claim() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (_dir, output_manager) = make_output_manager(fs.clone());
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let status_sink = Arc::new(RecordingStatusSink::default());
    let completion_sink = Arc::new(CountingCompletionSink::default());
    let handler = make_handler(
        NaturalAfterFinalizingExecutor::new(started_tx, release_rx),
        output_manager,
        status_sink.clone(),
        completion_sink.clone(),
    );
    let ctx = make_ctx(fs);
    let handle = handler
        .spawn(
            TaskSpawnInput::LocalFusion {
                request: dummy_request(),
                conversation_id: TEST_CONVERSATION_ID.into(),
            },
            ctx.clone(),
        )
        .await
        .expect("spawn succeeds");
    started_rx.await.expect("natural runner starts");

    let control = handler
        .workers
        .lock()
        .await
        .get(&handle.task_id)
        .expect("worker remains registered")
        .control
        .clone();
    assert!(control.begin_finalizing(), "test claims the natural result");
    handler
        .kill(&handle.task_id, ctx)
        .await
        .expect("kill losing finalizing arbitration is a no-op");
    let _ = release_tx.send(());

    for _ in 0..200 {
        if status_sink.last_status() == Some(TaskStatus::Completed) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(status_sink.last_status(), Some(TaskStatus::Completed));
    assert_eq!(completion_sink.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn drain_pending_kill_request_cancel_loses_to_an_atomic_finalizing_claim() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (_dir, output_manager) = make_output_manager(fs.clone());
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let status_sink = Arc::new(RecordingStatusSink::default());
    let completion_sink = Arc::new(CountingCompletionSink::default());
    let handler = make_handler(
        NaturalAfterFinalizingExecutor::new(started_tx, release_rx),
        output_manager,
        status_sink.clone(),
        completion_sink.clone(),
    );
    let ctx = make_ctx(fs);
    let handle = handler
        .spawn(
            TaskSpawnInput::LocalFusion {
                request: dummy_request(),
                conversation_id: TEST_CONVERSATION_ID.into(),
            },
            ctx,
        )
        .await
        .expect("spawn succeeds");
    started_rx.await.expect("natural runner starts");

    let control = handler
        .workers
        .lock()
        .await
        .get(&handle.task_id)
        .expect("worker remains registered")
        .control
        .clone();
    assert!(control.begin_finalizing(), "test claims the natural result");
    (handle.cleanup.as_ref().expect("cleanup seam"))();
    handler.drain_pending_kills().await;
    let _ = release_tx.send(());

    for _ in 0..200 {
        if status_sink.last_status() == Some(TaskStatus::Completed) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(status_sink.last_status(), Some(TaskStatus::Completed));
    assert_eq!(completion_sink.0.load(Ordering::SeqCst), 1);
}

/// [Finding 18]: the branch inserted a genuine suspension point —
/// `forwarder.await` — between `executor.run` returning `Ok(..)` and the
/// `finalizing` claim (contrast `kill_preserves_terminalizing_fusion_window`
/// above, which blocks strictly AFTER that claim). A `kill` landing while
/// the worker is parked on `forwarder.await` must see the same
/// "already finalizing" short-circuit as one landing after it — an
/// already-successful run's terminal side effects (egress+usage, the
/// `Completed` status, the published result) must survive, never end up as
/// a bare `Killed` with none of them run.
#[tokio::test]
async fn kill_landing_during_the_progress_forwarder_await_does_not_discard_a_successful_run() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (_dir, output_manager) = make_output_manager(fs.clone());
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let status_sink = Arc::new(BlockingStageSink::new(started_tx, release_rx));
    let completion_sink = Arc::new(CountingCompletionSink::default());
    let executor = SingleProgressExecutor::new();
    let handler = make_handler(
        executor.clone(),
        output_manager,
        status_sink.clone(),
        completion_sink.clone(),
    );
    let ctx = make_ctx(fs);

    let handle = handler
        .spawn(
            TaskSpawnInput::LocalFusion {
                request: dummy_request(),
                conversation_id: TEST_CONVERSATION_ID.into(),
            },
            ctx.clone(),
        )
        .await
        .expect("spawn succeeds");

    // The worker is now inside `forwarder.await`, blocked on the
    // forwarder's very first `set_fusion_stage` call for the lone buffered
    // progress event — i.e. AFTER `executor.run` already returned
    // `Ok(FusionResult)`.
    tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
        .await
        .expect("worker should reach the forwarder's stage call")
        .expect("stage-call signal");

    handler
        .kill(&handle.task_id, ctx)
        .await
        .expect("kill must not error");

    let _ = release_tx.send(());
    for _ in 0..200 {
        if status_sink
            .last_status()
            .is_some_and(TaskStatus::is_terminal)
        {
            break;
        }
        tokio::task::yield_now().await;
    }

    assert_eq!(
        status_sink.last_status(),
        Some(TaskStatus::Completed),
        "a kill landing while the worker awaits the progress forwarder must \
         not discard an already-successful Ok(FusionResult) as Killed"
    );
    assert_eq!(
        completion_sink.0.load(Ordering::SeqCst),
        1,
        "the completed result must still be published even when kill lands \
         inside the forwarder-await window"
    );
}

/// F012: `kill` on a run that is genuinely still executing (not already in
/// the terminalizing window) must let the executor observe cooperative
/// cancellation through its own inherited token before falling back to a
/// hard `runtime.cancel` abort, and must reach `Killed` exactly once —
/// never once from the worker's own `Err(FusionError::Cancelled)` arm AND
/// again from `kill`'s fallback.
#[tokio::test]
async fn kill_while_running_lets_executor_observe_cancellation_and_status_killed_once() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (_dir, output_manager) = make_output_manager(fs.clone());
    let status_sink = Arc::new(RecordingSink::default());
    let completion_sink = Arc::new(CountingCompletionSink::default());
    let executor = CancelAwareExecutor::new();
    let handler = make_handler(
        executor.clone(),
        output_manager,
        status_sink.clone(),
        completion_sink,
    );
    let ctx = make_ctx(fs);

    let handle = handler
        .spawn(
            TaskSpawnInput::LocalFusion {
                request: dummy_request(),
                conversation_id: TEST_CONVERSATION_ID.into(),
            },
            ctx.clone(),
        )
        .await
        .expect("spawn succeeds");

    // Wait until the worker has genuinely entered the executor and is
    // blocked on the inherited cancel token — not yet cancelled.
    for _ in 0..200 {
        if executor.runs.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        executor.runs.load(Ordering::SeqCst),
        1,
        "executor must have started"
    );
    assert_eq!(
        executor.observed_cancel.load(Ordering::SeqCst),
        0,
        "executor must still be genuinely blocked before kill"
    );

    handler
        .kill(&handle.task_id, ctx)
        .await
        .expect("kill succeeds");

    assert_eq!(
        executor.observed_cancel.load(Ordering::SeqCst),
        1,
        "kill must let the executor observe cooperative cancellation through its own \
         inherited token, not get hard-aborted mid-await"
    );
    assert_eq!(
        status_sink.count(TaskStatus::Killed),
        1,
        "status must reach Killed exactly once; got: {:?}",
        status_sink.calls()
    );
}

/// [Round-4 review finding 10] Killing a running `/fusion` whose executor
/// already reported real, already-billed usage on the progress channel
/// must still disclose that egress/usage on the terminal `Killed` row —
/// not silently drop it. `kill` removes the worker record from `workers`
/// BEFORE firing `cancel` (see `kill`'s own comment), so by the time
/// `executor.run` returns `Err(Cancelled)`, `finalize_fusion_outcome`'s
/// `Err(FusionError::Cancelled)` arm — and the `disclose_partial_usage` it
/// calls — is unreachable through the worker's own `may_finalize` check;
/// the fix recovers that disclosure on the early-return path instead.
#[tokio::test]
async fn kill_discloses_realized_usage_the_progress_channel_already_reported() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (_dir, output_manager) = make_output_manager(fs.clone());
    let status_sink = Arc::new(RecordingStatusSink::default());
    let completion_sink = Arc::new(CountingCompletionSink::default());
    let (started_tx, started_rx) = oneshot::channel();
    let executor = RealizedProgressBlockingExecutor::new(started_tx);
    let handler = make_handler(
        executor.clone(),
        output_manager,
        status_sink.clone(),
        completion_sink,
    );
    let ctx = make_ctx(fs);

    let handle = handler
        .spawn(
            TaskSpawnInput::LocalFusion {
                request: dummy_request(),
                conversation_id: TEST_CONVERSATION_ID.into(),
            },
            ctx.clone(),
        )
        .await
        .expect("spawn succeeds");

    started_rx
        .await
        .expect("executor must report progress and signal started before blocking");

    handler
        .kill(&handle.task_id, ctx)
        .await
        .expect("kill succeeds");

    // `kill` only waits for the worker's completion signal, which fires
    // once the worker future is fully dropped — poll briefly for the
    // disclosure write (`disclose_partial_usage`) that happens just before
    // that inside the worker's early-return path.
    for _ in 0..200 {
        if !status_sink.egress_and_usage().is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }

    let egress_and_usage = status_sink.egress_and_usage();
    assert!(
        !egress_and_usage.is_empty(),
        "the realized egress/usage the executor reported on the progress channel before \
being killed must still be recorded on the Killed row — got no \
set_fusion_egress_and_usage calls at all; events so far: {:?}",
        status_sink.events()
    );
    let (_, egress_profiles, usage) = &egress_and_usage[0];
    assert_eq!(
        egress_profiles,
        &vec!["anthropic".to_string(), "openai".to_string()],
        "must disclose the egress profiles the progress channel already reported"
    );
    assert_eq!(
        usage.as_ref().map(|u| u.subagent_tokens),
        Some(12),
        "must disclose the realized tokens the progress channel already reported"
    );
}

// ---- WP6 item 7: Err -> Failed with a recorded error, spool content,
// egress/usage recorded before the terminal transition ----------------------

#[tokio::test]
async fn failing_executor_records_error_before_failed_status_and_spools_it() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (_dir, output_manager) = make_output_manager(fs.clone());
    let status_sink = Arc::new(RecordingStatusSink::default());
    let completion_sink = Arc::new(CountingCompletionSink::default());
    let error = FusionError::TooFewModels {
        eligible: 1,
        required: 2,
        same_provider_only: false,
        parent_profile: "anthropic".into(),
    };
    let executor = Arc::new(FailingExecutor {
        error: error.clone(),
    });
    let handler = make_handler(
        executor,
        output_manager.clone(),
        status_sink.clone(),
        completion_sink.clone(),
    );

    let handle = handler
        .spawn(
            TaskSpawnInput::LocalFusion {
                request: dummy_request(),
                conversation_id: TEST_CONVERSATION_ID.into(),
            },
            make_ctx(fs.clone()),
        )
        .await
        .expect("spawn succeeds");

    for _ in 0..200 {
        if status_sink
            .last_status()
            .is_some_and(TaskStatus::is_terminal)
        {
            break;
        }
        tokio::task::yield_now().await;
    }

    // Status is Failed, exactly one error was recorded, and it carries the
    // real error text (not folded away into a generic "failed").
    assert_eq!(status_sink.last_status(), Some(TaskStatus::Failed));
    let errors = status_sink.errors();
    assert_eq!(errors.len(), 1, "exactly one error recorded: {errors:?}");
    assert_eq!(errors[0].1, error.to_string());
    // Ordering: the error must land BEFORE the terminal status flip so a
    // concurrent notification drain can never observe `Failed` with no error.
    assert_eq!(
        status_sink.events(),
        vec![
            "status:Running".to_string(),
            "egress_and_usage:0:true".to_string(),
            "receipt:NotRequired".to_string(),
            format!("error:{error}"),
            "status:Failed".to_string(),
        ]
    );
    // The Fusion run never published a completion (it failed).
    assert_eq!(completion_sink.0.load(Ordering::SeqCst), 0);

    // Spool contains the raw error text.
    let path = output_manager.path_for(&handle.task_id).unwrap();
    let spooled = fs
        .read_file(&path.to_string_lossy(), None, None)
        .await
        .expect("spool readable");
    assert_eq!(spooled.content, error.to_string());
}

/// [Finding 14]: a post-fan-out failure (`AllPanelsFailed`,
/// `MinPanelsNotMet`, `PanelSetIncomplete`, `TimedOutEmpty`) can follow real
/// panel spend — the orchestrator reports that spend on the progress
/// channel's `realized_output_tokens` field even as it returns `Err`. The
/// Err arm must disclose that best-effort usage (via
/// `set_fusion_egress_and_usage`) BEFORE the terminal `Failed` transition,
/// not leave the task's usage permanently `None` for a run that really
/// burned tokens.
#[tokio::test]
async fn failing_executor_that_already_spent_tokens_discloses_partial_usage_before_failed_status() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (_dir, output_manager) = make_output_manager(fs.clone());
    let status_sink = Arc::new(RecordingStatusSink::default());
    let completion_sink = Arc::new(CountingCompletionSink::default());
    let error = FusionError::AllPanelsFailed;
    let executor = Arc::new(PartialSpendThenFailExecutor {
        error: error.clone(),
        realized_output_tokens: 4_200,
        egress_profiles: Vec::new(),
    });
    let handler = make_handler(
        executor,
        output_manager,
        status_sink.clone(),
        completion_sink.clone(),
    );

    handler
        .spawn(
            TaskSpawnInput::LocalFusion {
                request: dummy_request(),
                conversation_id: TEST_CONVERSATION_ID.into(),
            },
            make_ctx(fs),
        )
        .await
        .expect("spawn succeeds");

    for _ in 0..200 {
        if status_sink
            .last_status()
            .is_some_and(TaskStatus::is_terminal)
        {
            break;
        }
        tokio::task::yield_now().await;
    }

    assert_eq!(status_sink.last_status(), Some(TaskStatus::Failed));
    let egress_and_usage = status_sink.egress_and_usage();
    assert_eq!(
        egress_and_usage.len(),
        1,
        "exactly one usage disclosure recorded on the failure path: {egress_and_usage:?}"
    );
    let usage = egress_and_usage[0]
        .2
        .clone()
        .expect("usage must be Some when tokens were realized before the failure");
    assert_eq!(
        usage.subagent_tokens, 4_200,
        "the realized output tokens the orchestrator reported before failing \
         must reach the task's disclosed usage"
    );
    // Ordering: the usage disclosure must land BEFORE the terminal status
    // flip, same rule as the error and the Ok-path egress/usage write.
    assert_eq!(
        status_sink.events(),
        vec![
            "status:Running".to_string(),
            "egress_and_usage:0:true".to_string(),
            "receipt:NotRequired".to_string(),
            format!("error:{error}"),
            "status:Failed".to_string(),
        ]
    );
}

/// [Round-3 review B2 / item 14 residual]: item 14's fix only filled in
/// `<usage>` on the failure path — `disclose_partial_usage` hardcoded
/// `Vec::new()` for the egress profiles, so a failing cross-provider run
/// (P1/P2 completed, P3 failed, `partial_ok: false`) still under-reported
/// the exact providers that received the user's prompt. This is the
/// privacy-relevant half: the grading bar is that a FAILING run's
/// `set_fusion_egress_and_usage` call carries the NON-EMPTY resolved
/// profile list, not just usage. `PartialSpendThenFailExecutor` here mirrors
/// the real orchestrator shape — a `Failed`-stage progress event carrying
/// both `realized_output_tokens` and `egress_profiles` right before the
/// `Err` return (see `fusion::orchestrator::run_inner`'s `check_panel_bar`
/// arm).
#[tokio::test]
async fn failing_executor_that_already_egressed_discloses_nonempty_egress_profiles() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (_dir, output_manager) = make_output_manager(fs.clone());
    let status_sink = Arc::new(RecordingStatusSink::default());
    let completion_sink = Arc::new(CountingCompletionSink::default());
    let error = FusionError::PanelSetIncomplete;
    let executor = Arc::new(PartialSpendThenFailExecutor {
        error: error.clone(),
        realized_output_tokens: 4_200,
        egress_profiles: vec!["openai".to_string(), "google".to_string()],
    });
    let handler = make_handler(
        executor,
        output_manager,
        status_sink.clone(),
        completion_sink.clone(),
    );

    handler
        .spawn(
            TaskSpawnInput::LocalFusion {
                request: dummy_request(),
                conversation_id: TEST_CONVERSATION_ID.into(),
            },
            make_ctx(fs),
        )
        .await
        .expect("spawn succeeds");

    for _ in 0..200 {
        if status_sink
            .last_status()
            .is_some_and(TaskStatus::is_terminal)
        {
            break;
        }
        tokio::task::yield_now().await;
    }

    assert_eq!(status_sink.last_status(), Some(TaskStatus::Failed));
    let egress_and_usage = status_sink.egress_and_usage();
    assert_eq!(
        egress_and_usage.len(),
        1,
        "exactly one usage disclosure recorded on the failure path: {egress_and_usage:?}"
    );
    // The defect this pins: item 14's fix left this a hardcoded
    // `Vec::new()` regardless of what the run actually egressed to.
    assert_eq!(
        egress_and_usage[0].1,
        vec!["openai".to_string(), "google".to_string()],
        "a failing run that really dispatched to openai and google must \
         disclose those profiles, not an empty list: {egress_and_usage:?}"
    );
    let usage = egress_and_usage[0]
        .2
        .clone()
        .expect("usage must still be Some when tokens were realized before the failure");
    assert_eq!(usage.subagent_tokens, 4_200);
}

#[tokio::test]
async fn ok_executor_records_egress_and_usage_before_completed_status() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (_dir, output_manager) = make_output_manager(fs.clone());
    let status_sink = Arc::new(RecordingStatusSink::default());
    let completion_sink = Arc::new(CountingCompletionSink::default());
    let executor = ImmediateExecutor::new();
    let handler = make_handler(
        executor.clone(),
        output_manager,
        status_sink.clone(),
        completion_sink.clone(),
    );

    handler
        .spawn(
            TaskSpawnInput::LocalFusion {
                request: dummy_request(),
                conversation_id: TEST_CONVERSATION_ID.into(),
            },
            make_ctx(fs),
        )
        .await
        .expect("spawn succeeds");

    for _ in 0..200 {
        if status_sink
            .last_status()
            .is_some_and(TaskStatus::is_terminal)
        {
            break;
        }
        tokio::task::yield_now().await;
    }

    assert_eq!(status_sink.last_status(), Some(TaskStatus::Completed));
    let egress_and_usage = status_sink.egress_and_usage.lock().unwrap().clone();
    assert_eq!(
        egress_and_usage.len(),
        1,
        "egress/usage recorded exactly once: {egress_and_usage:?}"
    );
    // `dummy_result()` carries no egress profiles and zeroed usage — assert
    // the concrete content the handler forwarded, not just "some value".
    assert_eq!(egress_and_usage[0].1, Vec::<String>::new());
    let usage = egress_and_usage[0].2.as_ref().expect("usage summary set");
    assert_eq!(usage.subagent_tokens, 0);
    assert_eq!(usage.tool_uses, 0);
    assert_eq!(usage.duration_ms, 0);
    // Ordering: egress/usage lands before the terminal `outcome` publish, and
    // the publication receipt fires last — after the completion sink's own
    // publish, which for `CountingCompletionSink` has already run by the time
    // we observe a terminal status (there is no real await point between them
    // here), so it is on this same log too.
    assert_eq!(
        status_sink.events(),
        vec![
            "status:Running".to_string(),
            "egress_and_usage:0:true".to_string(),
            "outcome".to_string(),
            "status:Completed".to_string(),
            "receipt:Published".to_string(),
        ]
    );
    assert_eq!(completion_sink.0.load(Ordering::SeqCst), 1);
}

// ---- WP6 item 7: kill-while-running and a sink that never gets to publish
// must not corrupt the terminal status ---------------------------------

#[tokio::test]
async fn kill_while_running_ends_status_killed_exactly_once() {
    // Full cooperative-cancellation OBSERVABILITY by the executor (proving
    // `inherit.cancel.cancelled()` resolves before the worker task is
    // aborted) is F012/WP8's kill-path fix, landing on this same file from a
    // different work package/lane — not re-verified here to avoid
    // conflicting with that in-flight change. This test covers what WP6 owns:
    // killing a task whose executor is genuinely still in-flight must still
    // converge on exactly one `Killed` transition, not hang and not leave
    // `workers` holding a stale entry a second `kill` could double-fire on.
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (_dir, output_manager) = make_output_manager(fs.clone());
    let (started_tx, started_rx) = oneshot::channel();
    let status_sink = Arc::new(RecordingStatusSink::default());
    let completion_sink = Arc::new(CountingCompletionSink::default());
    let executor = BlockingCancelAwareExecutor::new(started_tx);
    let handler = make_handler(
        executor.clone(),
        output_manager,
        status_sink.clone(),
        completion_sink.clone(),
    );
    let ctx = make_ctx(fs);

    let handle = handler
        .spawn(
            TaskSpawnInput::LocalFusion {
                request: dummy_request(),
                conversation_id: TEST_CONVERSATION_ID.into(),
            },
            ctx.clone(),
        )
        .await
        .expect("spawn succeeds");

    tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
        .await
        .expect("worker should enter the blocking executor")
        .expect("started signal");
    assert_eq!(
        executor.runs.load(Ordering::SeqCst),
        1,
        "the executor was genuinely invoked before kill"
    );

    handler
        .kill(&handle.task_id, ctx.clone())
        .await
        .expect("kill succeeds while the executor is in-flight");

    assert_eq!(status_sink.last_status(), Some(TaskStatus::Killed));
    let terminal_transitions = status_sink
        .statuses
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, s)| s.is_terminal())
        .count();
    assert_eq!(
        terminal_transitions,
        1,
        "exactly one terminal transition: {:?}",
        status_sink.statuses.lock().unwrap()
    );
    assert_eq!(
        completion_sink.0.load(Ordering::SeqCst),
        0,
        "a killed run must never publish a completion"
    );

    // A second kill on the now-terminal task is a harmless no-op, not a
    // double transition.
    handler
        .kill(&handle.task_id, ctx)
        .await
        .expect("kill is idempotent once terminal");
    assert_eq!(
        status_sink
            .statuses
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, s)| s.is_terminal())
            .count(),
        1,
        "kill after terminal must not add a second transition"
    );
}

#[tokio::test]
async fn a_sink_that_never_publishes_does_not_revert_a_completed_run_to_failed() {
    // `LocalFusionHandler`'s own doc comment (top of this file) states the
    // invariant: "Sink failures must not rewrite the task's terminal
    // status." This pins it with a sink that captures the status AT THE
    // MOMENT it is invoked (proving the terminal transition already
    // happened) and then does nothing further — the run must still read
    // back as Completed.
    struct FailingCompletionSink {
        calls: AtomicUsize,
        status_at_publish: StdMutex<Option<TaskStatus>>,
        status_sink: Arc<RecordingStatusSink>,
    }

    #[async_trait]
    impl FusionCompletionSink for FailingCompletionSink {
        async fn publish(
            &self,
            _conversation_id: &str,
            _result: &FusionResult,
        ) -> platform_api::FusionPublicationReceipt {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.status_at_publish.lock().unwrap() = self.status_sink.last_status();
            // Simulate a sink that fails to reach the client (e.g.
            // `DesktopFusionCompletionSink::publish`'s real `append_meta_..`
            // erroring) — it logs and returns, touching nothing else.
            platform_api::FusionPublicationReceipt::storage_failure("test completion sink failure")
        }
    }

    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (_dir, output_manager) = make_output_manager(fs.clone());
    let status_sink = Arc::new(RecordingStatusSink::default());
    let completion_sink = Arc::new(FailingCompletionSink {
        calls: AtomicUsize::new(0),
        status_at_publish: StdMutex::new(None),
        status_sink: status_sink.clone(),
    });
    let executor = ImmediateExecutor::new();
    let handler = make_handler(
        executor,
        output_manager,
        status_sink.clone(),
        completion_sink.clone(),
    );

    handler
        .spawn(
            TaskSpawnInput::LocalFusion {
                request: dummy_request(),
                conversation_id: TEST_CONVERSATION_ID.into(),
            },
            make_ctx(fs),
        )
        .await
        .expect("spawn succeeds");

    for _ in 0..200 {
        if status_sink
            .last_status()
            .is_some_and(TaskStatus::is_terminal)
        {
            break;
        }
        tokio::task::yield_now().await;
    }

    assert_eq!(completion_sink.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *completion_sink.status_at_publish.lock().unwrap(),
        Some(TaskStatus::Completed),
        "the terminal status must already be Completed by the time publish runs"
    );
    // The "failing" sink changed nothing: status stays Completed, not Failed.
    assert_eq!(status_sink.last_status(), Some(TaskStatus::Completed));
}

// ---- Round-7 items 5 & 6: `<subagent_tokens>` must mean the same thing for
// a `/fusion` run as it does for a `local_agent` run -------------------------

/// Returns a `FusionResult` whose `usage` has every bucket populated with a
/// DISTINCT value, so an assertion on the summed total can only pass if the
/// intended buckets — and only those — were added.
struct FullUsageExecutor;

#[async_trait]
impl FusionExecutor for FullUsageExecutor {
    fn prepare(
        self: ::std::sync::Arc<Self>,
        submission: ::platform_api::FusionSubmission,
    ) -> Result<::platform_api::PreparedFusionRun, ::platform_api::FusionError> {
        let timeout = self.effective_timeout_ms();
        ::platform_api::prepared_from_oneshot(
            submission,
            timeout,
            move |_request, _inherit, _progress| async move {
                Ok(FusionResult {
                    usage: FusionUsage {
                        input_tokens: 3_000,
                        output_tokens: 8_000,
                        reasoning_tokens: 90_000,
                        cache_read_tokens: 160_000,
                        cache_write_tokens: 40_000,
                        realized_nano_usd: 1,
                        reserved_max_nano_usd: 2,
                        estimated: false,
                        provider_requests: 7,
                    },
                    ..dummy_result()
                })
            },
        )
    }
}

/// Round-7 items 5+6 (one defect): `AgentRunUsage.subagent_tokens` is
/// main-owned and documented as claude-code's `totalTokens`
/// (`platform_api::task_registry::AgentRunUsage`), and main's only other
/// producer — `local_agent.rs`'s `input + cache_write + cache_read + output`
/// (mirrored in `agent::handle::subagent_usage_from_llm_usage`) — fills it
/// with all four billable buckets. `finalize_fusion_outcome` filled it with
/// `input + output` alone, so a cache-heavy `/fusion` run reported a small
/// fraction of the tokens an equivalent background-agent run reports under
/// the very same `<subagent_tokens>` tag.
///
/// The expected value deliberately EXCLUDES `reasoning_tokens`: `local_agent`
/// excludes `bt.reasoning_output` too (`cost::usage`'s `total_context_tokens`
/// mirrors TS's four-term sum), and the point of this fix is to match main's
/// definition of the tag, not to invent a third one.
#[tokio::test]
async fn completed_fusion_reports_subagent_tokens_as_local_agents_four_bucket_total() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let (_dir, output_manager) = make_output_manager(fs.clone());
    let status_sink = Arc::new(RecordingStatusSink::default());
    let completion_sink = Arc::new(CountingCompletionSink::default());
    let handler = make_handler(
        Arc::new(FullUsageExecutor),
        output_manager,
        status_sink.clone(),
        completion_sink,
    );

    handler
        .spawn(
            TaskSpawnInput::LocalFusion {
                request: dummy_request(),
                conversation_id: TEST_CONVERSATION_ID.into(),
            },
            make_ctx(fs),
        )
        .await
        .expect("spawn succeeds");

    for _ in 0..200 {
        if status_sink
            .last_status()
            .is_some_and(TaskStatus::is_terminal)
        {
            break;
        }
        tokio::task::yield_now().await;
    }

    let egress_and_usage = status_sink.egress_and_usage();
    let usage = egress_and_usage
        .first()
        .and_then(|(_, _, usage)| usage.clone())
        .expect("the Ok arm must record an AgentRunUsage");
    assert_eq!(
        usage.subagent_tokens, 211_000,
        "<subagent_tokens> must be input(3000) + cache_write(40000) + \
cache_read(160000) + output(8000) — the same four billable buckets \
local_agent.rs sums for this exact tag; got a total that drops the cache \
buckets Fusion already carries"
    );
    assert_eq!(
        usage.tool_uses, 7,
        "provider_requests must still map to <tool_uses> unchanged"
    );
}

#[tokio::test]
async fn error_facts_disclose_possible_egress_even_when_usage_is_unknown() {
    let status_sink = Arc::new(RecordingStatusSink::default());
    let sink: Arc<dyn TaskStatusSink> = status_sink.clone();
    let facts = FusionRunFacts {
        possible_egress: vec!["openai".into()],
        ..FusionRunFacts::default()
    };

    // No progress payload is available: the reliable possible-egress fact is
    // still projected, while usage remains explicitly unknown.
    disclose_facts_or_partial_usage(&sink, "task", &facts, None, None).await;

    let entries = status_sink.egress_and_usage();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].1, vec!["openai"]);
    assert!(entries[0].2.is_none());
}

#[tokio::test]
async fn error_facts_union_confirmed_and_possible_egress_without_duplicates() {
    let status_sink = Arc::new(RecordingStatusSink::default());
    let sink: Arc<dyn TaskStatusSink> = status_sink.clone();
    let facts = FusionRunFacts {
        confirmed_egress: vec!["openai".into()],
        possible_egress: vec!["anthropic".into(), "openai".into()],
        ..FusionRunFacts::default()
    };

    disclose_facts_or_partial_usage(&sink, "task", &facts, None, None).await;

    let entries = status_sink.egress_and_usage();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].1, vec!["anthropic", "openai"]);
    assert!(entries[0].2.is_none());
}
