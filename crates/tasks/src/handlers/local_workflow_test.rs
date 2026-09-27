//! Local workflow tests.
#![allow(clippy::unwrap_used)]

use super::*;
use platform_api::filesystem::{FileContent, FileEvent, FileSystem, FlockGuard, FsError};
use platform_api::tool_invoker::{SubagentInvocationContext, ToolInvokerError};
use platform_api::{
    BudgetError, FusionAgentSurface, FusionDecision, FusionError, FusionExecutor,
    FusionNeedsParentReason, FusionRequest, FusionResult, FusionStatus, FusionTiming, FusionUsage,
    PanelRunStatus, SubagentUsage,
};
use serde_json::json;
use std::any::Any;
use std::collections::HashMap as StdHashMap;
use std::collections::HashSet as StdHashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
use std::sync::Mutex as StdMutex;
use tempfile::tempdir;
use test_harness::mocks::MockRuntimeSpawner;
use tokio::sync::oneshot;
use tokio::sync::Mutex as TokioMutex;
use tokio_util::sync::CancellationToken;

use crate::handlers::CONFIG_DIR_ENV_LOCK as ENV_LOCK;

mod fusion_batch_contract_tests {
    use super::*;
    use platform_api::{
        ModelAttemptBillingMode, WorkflowOutputAccount, WorkflowOutputEventId, WorkflowOutputScope,
    };

    struct Output(protocol::SessionId, protocol::MessageId);
    impl WorkflowOutputAccount for Output {
        fn session_id(&self) -> protocol::SessionId {
            self.0
        }
        fn generation_id(&self) -> protocol::MessageId {
            self.1
        }
        fn spent(&self) -> u64 {
            0
        }
        fn record_legacy(&self, _: WorkflowOutputEventId, _: u64) -> Result<(), BudgetError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct TerminalRecorder(StdMutex<Vec<platform_api::FusionRunOutcome>>);

    #[async_trait]
    impl platform_api::FusionRunRecorder for TerminalRecorder {
        async fn record_terminal(
            &self,
            outcome: platform_api::FusionRunOutcome,
            slash_target: Option<platform_api::FusionSlashPublicationTarget>,
        ) -> platform_api::FusionPublicationReceipt {
            assert!(
                slash_target.is_none(),
                "workflow cannot publish a slash outbox"
            );
            self.0.lock().unwrap().push(outcome);
            platform_api::FusionPublicationReceipt::not_required()
        }
    }

    struct BatchExecutor {
        cap: usize,
        mode: ModelAttemptBillingMode,
        failure: Option<String>,
        events: mpsc::UnboundedSender<String>,
        gates: Vec<tokio::sync::Semaphore>,
        active: AtomicU32,
        maximum: AtomicU32,
        started: AtomicU32,
        journal: Arc<StdMutex<HashMap<String, String>>>,
        recorder: Arc<TerminalRecorder>,
    }
    struct Active(Arc<BatchExecutor>);
    impl Drop for Active {
        fn drop(&mut self) {
            self.0.active.fetch_sub(1, AtomicOrdering::SeqCst);
        }
    }
    impl BatchExecutor {
        fn new(
            cap: usize,
            mode: ModelAttemptBillingMode,
            failure: Option<String>,
            open: bool,
        ) -> (Arc<Self>, mpsc::UnboundedReceiver<String>) {
            let (events, receiver) = mpsc::unbounded_channel();
            (
                Arc::new(Self {
                    cap,
                    mode,
                    failure,
                    events,
                    gates: (0..21)
                        .map(|_| tokio::sync::Semaphore::new(usize::from(open)))
                        .collect(),
                    active: AtomicU32::new(0),
                    maximum: AtomicU32::new(0),
                    started: AtomicU32::new(0),
                    journal: Arc::new(StdMutex::new(HashMap::new())),
                    recorder: Arc::new(TerminalRecorder::default()),
                }),
                receiver,
            )
        }
    }
    #[async_trait]
    impl FusionExecutor for BatchExecutor {
        fn workflow_batch_concurrency(&self) -> usize {
            self.cap
        }
        fn agent_surface(&self) -> FusionAgentSurface {
            FusionAgentSurface {
                enabled: true,
                ..Default::default()
            }
        }
        fn prepare(
            self: Arc<Self>,
            submission: FusionSubmission,
        ) -> Result<PreparedFusionRun, FusionError> {
            let prompt = submission.request.prompt;
            let index = prompt.parse::<usize>().unwrap();
            let control = platform_api::FusionRunControl::new_with_billing_mode(
                submission.identity.clone(),
                10_000,
                submission.inherit.cancel,
                FusionRunFactsRecorder::default(),
                self.mode,
            );
            Ok(PreparedFusionRun::new(
                FusionPreparedSummary {
                    identity: submission.identity,
                    duration_ms: 10_000,
                    planned_panels: Some(2),
                },
                control.clone(),
                move |_, _| async move {
                    self.started.fetch_add(1, AtomicOrdering::SeqCst);
                    let active = self.active.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                    self.maximum.fetch_max(active, AtomicOrdering::SeqCst);
                    let _active = Active(self.clone());
                    let _ = self.events.send(format!("start:{index}"));
                    let _permit = self.gates[index].acquire().await.unwrap();
                    if self.mode == ModelAttemptBillingMode::MeteredAttempts {
                        control.facts().set_attempt_settlement(
                            if self.failure.as_ref() == Some(&prompt) {
                                platform_api::FusionAttemptSettlementStatus::Failed {
                                    reason: "test receipt failure".into(),
                                }
                            } else {
                                platform_api::FusionAttemptSettlementStatus::Settled
                            },
                        );
                    }
                    let mut result = workflow_fusion_result();
                    result.run_id = control.identity().run_id.to_string();
                    result.final_text = prompt;
                    let _ = self.events.send(format!("done:{index}"));
                    platform_api::FusionRunOutcome::from_control(&control, Ok(result))
                },
            ))
        }
    }

    async fn run_batch(
        executor: Arc<BatchExecutor>,
        scope: bool,
        count: usize,
    ) -> Result<workflow::RunOutcome, workflow::WorkflowError> {
        let session = protocol::SessionId::new();
        let recorder = executor.recorder.clone();
        let script = format!("return await Promise.allSettled(Array.from({{length:{count}}}, (_, i) => fusion(String(i))));");
        run_workflow_script_with_live_updates_and_fusion_recorded(
            &script,
            DEFAULT_WORKFLOW_SUBAGENT,
            "batch",
            Arc::new(EchoSpawner::default()),
            Arc::new(MockInvoker),
            Arc::new(MockBudget),
            None,
            None,
            Some(executor.journal.clone()),
            None,
            Some(100),
            None,
            0,
            NestedConfig {
                session_uuid: Some(session.to_string()),
                ..Default::default()
            },
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            CancellationToken::new(),
            Some(executor),
            Some("batch".into()),
            Some("model".into()),
            Some("profile".into()),
            None,
            Arc::new(AnalyticsBus::new()),
            None,
            None,
            None,
            Some(recorder),
            scope.then(|| {
                WorkflowOutputScope::new(Arc::new(Output(session, protocol::MessageId::new())))
            }),
        )
        .await
    }

    async fn event(events: &mut mpsc::UnboundedReceiver<String>) -> String {
        tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
            .await
            .expect("bounded batch progress")
            .unwrap()
    }

    #[tokio::test]
    async fn workflow_fusion_batch_two_active_third_waits_and_results_stay_ordered() {
        let (executor, mut events) =
            BatchExecutor::new(2, ModelAttemptBillingMode::MeteredAttempts, None, false);
        let run = tokio::spawn(run_batch(executor.clone(), true, 3));
        assert_eq!(event(&mut events).await, "start:0");
        assert_eq!(event(&mut events).await, "start:1");
        assert!(events.try_recv().is_err());
        executor.gates[1].add_permits(1);
        assert_eq!(event(&mut events).await, "done:1");
        assert_eq!(executor.started.load(AtomicOrdering::SeqCst), 2);
        executor.gates[0].add_permits(1);
        assert_eq!(event(&mut events).await, "done:0");
        assert_eq!(event(&mut events).await, "start:2");
        executor.gates[2].add_permits(1);
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let values: Value = serde_json::from_str(outcome.result.as_deref().unwrap()).unwrap();
        for index in 0..3 {
            assert_eq!(values[index]["value"]["final_text"], index.to_string());
        }
        assert_eq!(executor.maximum.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(executor.active.load(AtomicOrdering::SeqCst), 0);
    }

    #[tokio::test]
    async fn workflow_fusion_batch_rollback_one_keeps_calls_serial() {
        for mode in [
            ModelAttemptBillingMode::LegacyAggregate,
            ModelAttemptBillingMode::MeteredAttempts,
        ] {
            let (executor, mut events) = BatchExecutor::new(1, mode, None, false);
            let run = tokio::spawn(run_batch(executor.clone(), true, 3));
            for index in 0..3 {
                assert_eq!(event(&mut events).await, format!("start:{index}"));
                assert!(events.try_recv().is_err());
                executor.gates[index].add_permits(1);
                assert_eq!(event(&mut events).await, format!("done:{index}"));
            }
            run.await.unwrap().unwrap();
            assert_eq!(executor.maximum.load(AtomicOrdering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn workflow_fusion_batch_two_refuses_legacy_or_missing_scope_before_activation() {
        for (mode, scope) in [
            (ModelAttemptBillingMode::LegacyAggregate, true),
            (ModelAttemptBillingMode::MeteredAttempts, false),
        ] {
            let (executor, _) = BatchExecutor::new(2, mode, None, true);
            let outcome = run_batch(executor.clone(), scope, 3).await.unwrap();
            let values: Value = serde_json::from_str(outcome.result.as_deref().unwrap()).unwrap();
            assert!(values
                .as_array()
                .unwrap()
                .iter()
                .all(|value| value["status"] == "rejected"));
            assert_eq!(executor.started.load(AtomicOrdering::SeqCst), 0);
            let recorded = executor.recorder.0.lock().unwrap();
            assert_eq!(
                recorded.len(),
                3,
                "exactly one terminal refusal per queued call"
            );
            let identities: StdHashSet<_> = recorded
                .iter()
                .map(|outcome| outcome.identity.run_id.to_string())
                .collect();
            assert_eq!(
                identities.len(),
                3,
                "terminal refusals must not be duplicated"
            );
            for outcome in recorded.iter() {
                assert!(outcome.result.is_err());
                assert_eq!(outcome.identity.origin, FusionOrigin::Workflow);
                assert_eq!(outcome.facts.allocated_panels, Some(0));
                assert_eq!(outcome.facts.attempts, Some(0));
            }
        }
    }

    #[tokio::test]
    async fn workflow_fusion_batch_failed_receipt_stops_next_call_and_drains_inflight() {
        let (executor, mut events) = BatchExecutor::new(
            2,
            ModelAttemptBillingMode::MeteredAttempts,
            Some("1".into()),
            false,
        );
        let run = tokio::spawn(run_batch(executor.clone(), true, 3));
        assert_eq!(event(&mut events).await, "start:0");
        assert_eq!(event(&mut events).await, "start:1");
        executor.gates[1].add_permits(1);
        assert_eq!(event(&mut events).await, "done:1");
        // The supervisor has completed; wait for the bridge to consume its
        // failure and journal the retained answer before releasing call 0.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !executor
                .journal
                .lock()
                .unwrap()
                .values()
                .any(|result| result.contains("test receipt failure"))
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        executor.gates[0].add_permits(1);
        let outcome = run.await.unwrap().unwrap();
        let values: Value = serde_json::from_str(outcome.result.as_deref().unwrap()).unwrap();
        assert_eq!(values[1]["value"]["attempt_settlement"]["status"], "failed");
        assert_eq!(values[2]["status"], "rejected");
        assert_eq!(executor.started.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(executor.active.load(AtomicOrdering::SeqCst), 0);
    }

    #[tokio::test]
    async fn workflow_fusion_batch_twenty_one_calls_preserves_hard_cap() {
        let (executor, _) =
            BatchExecutor::new(2, ModelAttemptBillingMode::MeteredAttempts, None, true);
        let outcome = run_batch(executor.clone(), true, 21).await.unwrap();
        let values: Value = serde_json::from_str(outcome.result.as_deref().unwrap()).unwrap();
        assert_eq!(values.as_array().unwrap().len(), 21);
        assert!(values.as_array().unwrap()[..20]
            .iter()
            .all(|value| value["status"] == "fulfilled"));
        assert_eq!(values[20]["status"], "rejected");
        assert_eq!(executor.started.load(AtomicOrdering::SeqCst), 20);
    }
}

#[tokio::test]
async fn nested_name_resolves_plugin_snapshot_after_saved_miss() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let registry = workflow::PluginWorkflowRegistry::new();
    let script_dir = tempdir().unwrap();
    let script_path = script_dir.path().join("nested.js");
    let script =
        "export const meta = { name: 'nested', description: 'nested plugin' };\nreturn 7;\n";
    std::fs::write(&script_path, script).expect("seed plugin script");
    registry.register(vec![workflow::PluginWorkflowEntry {
        name: "acme:nested".to_string(),
        script_path,
    }]);

    let resolved =
        resolve_nested_script(&json!({"name": "acme:nested"}), Some(&fs), Some(&registry))
            .await
            .expect("nested plugin workflow");
    assert_eq!(resolved, script);
}

#[test]
fn terminal_metrics_distinguish_done_error_skipped_and_empty_results() {
    assert!(workflow_result_value_is_empty(&json!("")));
    assert!(workflow_result_value_is_empty(&json!([])));
    assert!(workflow_result_value_is_empty(&json!({})));
    assert!(workflow_result_value_is_empty(&json!({"items": []})));
    assert!(!workflow_result_value_is_empty(&json!({"items": [1]})));

    let mut metrics = WorkflowRunMetrics::default();
    metrics.record_cached(0, None, None, "[]");
    metrics.record_result(
        1,
        None,
        None,
        &Ok(SubagentResult::Completed {
            agent_id: protocol::AgentId::new(),
            content: json!("answer"),
            usage: SubagentUsage::default(),
            total_tool_use_count: 0,
            total_duration_ms: 0,
            total_tokens: 0,
            assistant_message_count: 0,
            response_char_count: 0,
            last_request_id: None,
            cumulative_usage: SubagentUsage::default(),
            usage_complete: true,
        }),
    );
    metrics.record_result(
        2,
        None,
        None,
        &Ok(SubagentResult::Failed {
            agent_id: protocol::AgentId::new(),
            reason: "boom".into(),
            usage: SubagentUsage::default(),
        }),
    );
    metrics.record_result(
        3,
        None,
        None,
        &Ok(SubagentResult::Failed {
            agent_id: protocol::AgentId::new(),
            reason: "skipped by user".into(),
            usage: SubagentUsage::default(),
        }),
    );

    assert_eq!(metrics.terminal_counts(), (2, 1, 1, 1));
}

#[test]
fn local_app_workflow_lease_root_is_derived_from_the_requested_app() {
    let data_root = PathBuf::from("/profile");
    assert_eq!(
        local_app_workspace_root(&data_root, "app-a"),
        PathBuf::from("/profile/apps/app-a/workspace")
    );
    assert_eq!(
        local_app_workspace_root(&data_root, "app-b"),
        PathBuf::from("/profile/apps/app-b/workspace")
    );
    assert_ne!(
        local_app_workspace_root(&data_root, "app-a"),
        local_app_workspace_root(&data_root, "app-b")
    );
}

/// The lease is what stops a build from borrowing the session cwd or another
/// app's workspace. `requires_workspace_lease` now reads a task's typed
/// `LocalAppWorkflowTaskScope` (design §18 Phase -1 step 8 / §8.1) instead of
/// a `workflow_id` name check, so a `Build`-purpose scope requires the lease
/// regardless of which app it names, and any other purpose never does.
#[test]
fn only_a_build_purpose_scope_requires_a_workspace_lease() {
    let build = crate::scope::LocalAppWorkflowTaskScope::for_build("app-a").expect("valid");
    let canvas_build =
        crate::scope::LocalAppWorkflowTaskScope::for_build("canvas-app").expect("valid");
    assert!(
        requires_workspace_lease(Some(&build)),
        "a Build scope always requires the lease, whatever app it names"
    );
    assert!(
        requires_workspace_lease(Some(&canvas_build)),
        "the drawn-surface build writes the same workspace and needs the same lease"
    );

    let use_test = crate::scope::LocalAppWorkflowTaskScope::for_use_test("app-a").expect("valid");
    let mcp = crate::scope::LocalAppWorkflowTaskScope::for_mcp_authoring("app-a").expect("valid");
    assert!(!requires_workspace_lease(Some(&use_test)));
    assert!(!requires_workspace_lease(Some(&mcp)));
}

/// §8.1 / hazard (d): a custom workflow that merely reuses a real build
/// workflow's `workflow_id` string carries no authority any more -- there is
/// no `workflow_id` parameter for it to reuse in the first place.
/// `requires_workspace_lease` takes only `Option<&LocalAppWorkflowTaskScope>`,
/// and a `None` -- which is what every workflow gets today, since nothing
/// yet threads a Host-minted scope through `spawn()` (see
/// `crate::state::LocalWorkflowTaskState::scope`'s doc comment) -- never
/// requires the lease, independent of any name.
#[test]
fn a_same_named_custom_workflow_gets_no_workspace_lease() {
    assert!(
        !requires_workspace_lease(None),
        "no scope at all must never grant the workspace lease, no matter what \
         workflow_id/args a caller attached to the task"
    );
}

// ---- Echo SubagentSpawner: `agent(p)` → "echo:p" (records prompts) ------

#[derive(Default)]
struct EchoSpawner {
    seen: StdMutex<Vec<String>>,
    seen_reqs: StdMutex<Vec<SubagentSpawnRequest>>,
    inherited_budget_totals: Option<Arc<StdMutex<Vec<u64>>>>,
    fail: bool,
    total_tokens: u64,
    total_tool_use_count: u64,
    total_duration_ms: u64,
}

#[derive(Default)]
struct WorkflowForwardingProbeSpawner {
    plain_spawns: std::sync::atomic::AtomicUsize,
    watchdogs: StdMutex<Vec<platform_api::subagent_spawn::WorkflowQueryWatchdog>>,
    observer_presence: StdMutex<Vec<bool>>,
}

struct BlockingWorkflowObserverSpawner {
    started: std::sync::atomic::AtomicUsize,
    started_prompts: StdMutex<Vec<String>>,
    released: std::sync::atomic::AtomicBool,
    release: tokio::sync::Notify,
    started_tx: StdMutex<Option<mpsc::UnboundedSender<String>>>,
}

impl BlockingWorkflowObserverSpawner {
    fn new(started_tx: mpsc::UnboundedSender<String>) -> Self {
        Self {
            started: std::sync::atomic::AtomicUsize::new(0),
            started_prompts: StdMutex::new(Vec::new()),
            released: std::sync::atomic::AtomicBool::new(false),
            release: tokio::sync::Notify::new(),
            started_tx: StdMutex::new(Some(started_tx)),
        }
    }

    fn started_count(&self) -> usize {
        self.started.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn release_all(&self) {
        self.released
            .store(true, std::sync::atomic::Ordering::Release);
        self.release.notify_waiters();
    }

    async fn wait_until_released(&self) {
        while !self.released.load(std::sync::atomic::Ordering::Acquire) {
            self.release.notified().await;
        }
    }
}

fn completed_probe_result(agent_id: protocol::AgentId) -> SubagentResult {
    SubagentResult::Completed {
        agent_id,
        content: Value::String("done".to_string()),
        usage: SubagentUsage::default(),
        total_tool_use_count: 0,
        total_duration_ms: 0,
        total_tokens: 0,
        assistant_message_count: 0,
        response_char_count: 0,
        last_request_id: None,
        cumulative_usage: SubagentUsage::default(),
        usage_complete: true,
    }
}

#[async_trait]
impl SubagentSpawner for WorkflowForwardingProbeSpawner {
    async fn spawn(
        &self,
        _request: SubagentSpawnRequest,
        _inherit: SubagentInheritance,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.plain_spawns
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(completed_probe_result(protocol::AgentId::new()))
    }

    async fn spawn_workflow_with_observer(
        &self,
        request: SubagentSpawnRequest,
        _inherit: SubagentInheritance,
        _progress: Option<tokio::sync::mpsc::Sender<String>>,
        observer: Option<Arc<dyn platform_api::subagent_spawn::SubagentSpawnObserver>>,
        watchdog: platform_api::subagent_spawn::WorkflowQueryWatchdog,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.watchdogs.lock().unwrap().push(watchdog);
        self.observer_presence
            .lock()
            .unwrap()
            .push(observer.is_some());
        let agent_id = protocol::AgentId::new();
        if let Some(observer) = observer {
            observer
                .on_event(
                    platform_api::subagent_spawn::SubagentObservation::Allocated {
                        agent_id,
                        agent_type: request.subagent_type,
                        name: request.name,
                        model: request.model.unwrap_or_else(|| "inherited".to_string()),
                        model_profile: request.model_profile,
                        persistent: false,
                        initial_message_index: 0,
                        origin_session_id: None,
                    },
                )
                .await;
        }
        Ok(completed_probe_result(agent_id))
    }
}

struct ImmediateFusionExecutor {
    surface: FusionAgentSurface,
    cap: u32,
    seen: StdMutex<Vec<FusionRequest>>,
    /// The session each run was prepared under. It travels on the trusted
    /// identity, never on the request.
    seen_sessions: StdMutex<Vec<Option<protocol::SessionId>>>,
    response: StdMutex<Result<FusionResult, FusionError>>,
}

impl ImmediateFusionExecutor {
    fn new(
        surface: FusionAgentSurface,
        cap: u32,
        response: Result<FusionResult, FusionError>,
    ) -> Arc<Self> {
        Arc::new(Self {
            surface,
            cap,
            seen: StdMutex::new(Vec::new()),
            seen_sessions: StdMutex::new(Vec::new()),
            response: StdMutex::new(response),
        })
    }
}

#[async_trait]
impl FusionExecutor for ImmediateFusionExecutor {
    fn prepare(
        self: ::std::sync::Arc<Self>,
        submission: ::platform_api::FusionSubmission,
    ) -> Result<::platform_api::PreparedFusionRun, ::platform_api::FusionError> {
        let this = ::std::sync::Arc::clone(&self);
        let timeout = self.effective_timeout_ms();
        self.seen_sessions
            .lock()
            .unwrap()
            .push(submission.identity.session_id);
        ::platform_api::prepared_from_oneshot(
            submission,
            timeout,
            move |request, _inherit, _progress| async move {
                this.seen.lock().unwrap().push(request);
                this.response.lock().unwrap().clone()
            },
        )
    }

    fn agent_surface(&self) -> FusionAgentSurface {
        self.surface
    }

    fn workflow_fusion_call_cap(&self) -> u32 {
        self.cap
    }

    fn resolve_parent_profile(
        &self,
        parent_model: &str,
        explicit_profile: Option<&str>,
    ) -> Option<String> {
        explicit_profile
            .map(str::to_string)
            .or_else(|| (parent_model == "gpt-5.4").then(|| "openai".into()))
    }
}

/// Finding [12]: mirrors `fusion::orchestrator::run_inner`'s
/// `check_panel_bar`-failure path — sends ONE `FusionProgress` event
/// carrying `realized_output_tokens` (the real, already-billed panel spend)
/// on the progress channel, THEN returns `Err`. Used to prove the
/// `local_workflow` `fusion()` bridge arm charges that spend against the
/// workflow's token budget even though the overall call errors.
struct FailingAfterRealSpendFusionExecutor {
    realized_output_tokens: u64,
}

#[async_trait]
impl FusionExecutor for FailingAfterRealSpendFusionExecutor {
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
                if let Some(tx) = &progress {
                    let _ = tx
                        .send(platform_api::FusionProgress {
                            stage: platform_api::FusionStage::Failed,
                            panel_id: None,
                            message: "panel bar failed after real provider spend".into(),
                            realized_output_tokens: Some(this.realized_output_tokens),
                            egress_profiles: None,
                            panels_allocated: None,
                        })
                        .await;
                }
                Err(FusionError::MinPanelsNotMet)
            },
        )
    }

    fn agent_surface(&self) -> FusionAgentSurface {
        FusionAgentSurface {
            enabled: true,
            default_partial_ok: true,
            ..FusionAgentSurface::default()
        }
    }
}

struct BlockingFusionExecutor {
    started: StdMutex<Option<oneshot::Sender<()>>>,
    cancelled: StdMutex<Option<oneshot::Sender<()>>>,
    seen: StdMutex<Vec<FusionRequest>>,
    cap: u32,
    calls: AtomicU32,
}

impl BlockingFusionExecutor {
    fn new(started: oneshot::Sender<()>, cancelled: oneshot::Sender<()>) -> Arc<Self> {
        Arc::new(Self {
            started: StdMutex::new(Some(started)),
            cancelled: StdMutex::new(Some(cancelled)),
            seen: StdMutex::new(Vec::new()),
            cap: 20,
            calls: AtomicU32::new(0),
        })
    }
}

#[async_trait]
impl FusionExecutor for BlockingFusionExecutor {
    fn prepare(
        self: ::std::sync::Arc<Self>,
        submission: ::platform_api::FusionSubmission,
    ) -> Result<::platform_api::PreparedFusionRun, ::platform_api::FusionError> {
        let this = ::std::sync::Arc::clone(&self);
        let timeout = self.effective_timeout_ms();
        ::platform_api::prepared_from_oneshot(
            submission,
            timeout,
            move |request, inherit, _progress| async move {
                this.calls.fetch_add(1, AtomicOrdering::SeqCst);
                this.seen.lock().unwrap().push(request);
                if let Some(tx) = this.started.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                inherit.cancel.cancelled().await;
                if let Some(tx) = this.cancelled.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                Err(FusionError::Cancelled)
            },
        )
    }

    fn agent_surface(&self) -> FusionAgentSurface {
        FusionAgentSurface {
            enabled: true,
            ..FusionAgentSurface::default()
        }
    }

    fn workflow_fusion_call_cap(&self) -> u32 {
        self.cap
    }
}

fn workflow_fusion_result() -> FusionResult {
    FusionResult {
        schema_version: 1,
        run_id: "fu_test".into(),
        status: FusionStatus::NeedsParent,
        decision: FusionDecision::NeedsParent {
            reason: FusionNeedsParentReason::LowConfidence,
        },
        final_text: "needs parent".into(),
        analysis: None,
        panels: vec![platform_api::PanelOutcome {
            panel_id: "P1".into(),
            status: PanelRunStatus::Completed,
            duration_ms: 7,
            error_category: None,
            error_detail: None,
            usage: Some(FusionUsage::default()),
        }],
        usage: FusionUsage::default(),
        timing: FusionTiming::default(),
        egress_profiles: vec!["openai".into()],
    }
}

fn workflow_fusion_result_with_output_tokens(output_tokens: u64) -> FusionResult {
    FusionResult {
        usage: FusionUsage {
            output_tokens,
            ..FusionUsage::default()
        },
        ..workflow_fusion_result()
    }
}

#[test]
fn workflow_fusion_uses_runtime_preset_and_resolves_a_missing_parent_profile() {
    let executor: Arc<dyn FusionExecutor> = ImmediateFusionExecutor::new(
        FusionAgentSurface {
            enabled: true,
            default_preset: platform_api::FusionPreset::Fast,
            ..FusionAgentSurface::default()
        },
        3,
        Ok(workflow_fusion_result()),
    );

    let request = parse_workflow_fusion_request(
        Some(&executor),
        "review this",
        "{}",
        "wf_fusion",
        Some("gpt-5.4"),
        None,
    )
    .expect("runtime defaults and provider fallback");

    assert_eq!(request.preset, platform_api::FusionPreset::Fast);
    assert_eq!(request.parent_profile, "openai");
}

#[test]
fn workflow_fusion_rejects_an_empty_parent_run_id_before_preparation() {
    let executor: Arc<dyn FusionExecutor> = ImmediateFusionExecutor::new(
        FusionAgentSurface {
            enabled: true,
            ..FusionAgentSurface::default()
        },
        3,
        Ok(workflow_fusion_result()),
    );

    let error = parse_workflow_fusion_request(
        Some(&executor),
        "review this",
        "{}",
        "  ",
        Some("gpt-5.4"),
        Some("openai"),
    )
    .expect_err("an empty workflow run id must fail before prepare");
    assert!(
        matches!(error, FusionError::InvalidRequest(message) if message.contains("non-empty workflow run id"))
    );
}

/// Mirrors `RejectedFusionExecutor` (apps/engine-desktop) — a
/// composition-root-pinned rejection surfaced through `preflight_error()`
/// while `agent_surface().enabled` stays `false`.
struct PreflightRejectedFusionExecutor {
    error: FusionError,
    // [R4-12] Call counter — a journal-cache hit must replay without ever
    // invoking `run()`, even when this executor's `preflight_error()` would
    // reject a freshly-parsed request. Zero-initialized by every existing
    // caller (`Default`).
    calls: AtomicU32,
}

impl Default for PreflightRejectedFusionExecutor {
    fn default() -> Self {
        Self {
            error: FusionError::InvalidConfiguration(String::new()),
            calls: AtomicU32::new(0),
        }
    }
}

#[async_trait]
impl FusionExecutor for PreflightRejectedFusionExecutor {
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
                this.calls.fetch_add(1, AtomicOrdering::SeqCst);
                Err(this.error.clone())
            },
        )
    }

    fn preflight_error(&self) -> Option<FusionError> {
        Some(self.error.clone())
    }
}

/// F008: `parse_workflow_fusion_request` must surface a composition-root-
/// pinned `preflight_error()` AS ITSELF, before the `agent_surface().enabled`
/// gate — otherwise a workflow's `fusion()` call would see the generic
/// `FusionError::Disabled` (every OTHER disabled-executor path) instead of
/// the real `InvalidConfiguration` a boot-time invalid `fusion.*` setting
/// produced.
#[test]
fn workflow_fusion_preflight_error_surfaces_before_the_disabled_gate() {
    let executor: Arc<dyn FusionExecutor> = Arc::new(PreflightRejectedFusionExecutor {
        error: FusionError::InvalidConfiguration("fusion.maxPanel must be between 1 and 12".into()),
        ..Default::default()
    });

    let err = parse_workflow_fusion_request(
        Some(&executor),
        "review this",
        "{}",
        "wf_fusion",
        Some("gpt-5.4"),
        None,
    )
    .expect_err("a preflight-rejected executor must fail");

    assert!(
        matches!(&err, FusionError::InvalidConfiguration(msg) if msg == "fusion.maxPanel must be between 1 and 12"),
        "expected the preflight InvalidConfiguration to pass through unchanged, got {err:?}"
    );
}

/// [R4-18] The workflow entry's structured `models` option
/// (`WorkflowFusionOpts.models`) bypasses the shared
/// `platform_api::parse_fusion_model_ref` the CLI (`/fusion --models`) and
/// Agent-tool string entrypoints both route through. Without an emptiness
/// check on the already-structured form, `{profile: "", model: "gpt-5.4"}`
/// used to reach `model_resolver::resolve_custom` unrejected and surface as
/// an unrelated `CrossProviderDenied`, and `{model: ""}` as an ``unknown
/// model `<profile>/` `` lookup failure — instead of the accurate
/// malformed-entry error the string entrypoints give for the equivalent
/// `":gpt-5.4"` / `"profile:"`.
#[test]
fn workflow_fusion_rejects_an_empty_profile_or_model_in_the_structured_models_option() {
    let executor: Arc<dyn FusionExecutor> = ImmediateFusionExecutor::new(
        FusionAgentSurface {
            enabled: true,
            ..FusionAgentSurface::default()
        },
        3,
        Ok(workflow_fusion_result()),
    );

    let empty_profile_err = parse_workflow_fusion_request(
        Some(&executor),
        "pick one",
        r#"{"models":[{"profile":"","model":"gpt-5.4"}]}"#,
        "wf_fusion",
        Some("gpt-5.4"),
        Some("openai"),
    )
    .expect_err(
        "an empty profile must be rejected as a malformed models entry, \
         not silently reach cross-provider resolution",
    );
    assert!(
        matches!(&empty_profile_err, FusionError::InvalidRequest(msg) if msg.contains("invalid fusion models entry")),
        "expected a malformed-entry InvalidRequest, got {empty_profile_err:?}"
    );

    let empty_model_err = parse_workflow_fusion_request(
        Some(&executor),
        "pick one",
        r#"{"models":[{"model":""}]}"#,
        "wf_fusion",
        Some("gpt-5.4"),
        Some("openai"),
    )
    .expect_err("an empty model must be rejected as a malformed models entry");
    assert!(
        matches!(&empty_model_err, FusionError::InvalidRequest(msg) if msg.contains("invalid fusion models entry")),
        "expected a malformed-entry InvalidRequest, got {empty_model_err:?}"
    );

    // A well-formed entry must still parse through unaffected.
    //
    // [R7-3/R7-7] The fixture carries TWO entries now, not one. The
    // single-entry list this used to assert on could never have run:
    // `fusion::model_resolver::resolve_custom` rejects any explicit list
    // below `FUSION_MIN_PANEL` ("explicit models must contain at least 2
    // entries"), so the old fixture pinned "parse accepts a request the
    // executor is certain to reject" — precisely the gap that let a
    // one-entry `models` list reach JS as a plain `Error` instead of
    // `WorkflowFusionOptionError`. The assertion itself is unchanged in
    // strength: a well-formed list still parses, and its structured refs
    // still round-trip verbatim.
    let ok = parse_workflow_fusion_request(
        Some(&executor),
        "pick one",
        r#"{"models":[{"profile":"openai","model":"gpt-5.4"},{"model":"o5-pro"}]}"#,
        "wf_fusion",
        Some("gpt-5.4"),
        Some("openai"),
    )
    .expect("a well-formed models entry must still parse");
    assert_eq!(
        ok.models,
        Some(vec![
            FusionModelRef {
                profile: Some("openai".into()),
                model: "gpt-5.4".into(),
            },
            FusionModelRef {
                profile: None,
                model: "o5-pro".into(),
            },
        ])
    );
}

#[async_trait]
impl SubagentSpawner for BlockingWorkflowObserverSpawner {
    async fn agent_listing(&self) -> Vec<platform_api::subagent_spawn::SubagentListingEntry> {
        vec![platform_api::subagent_spawn::SubagentListingEntry {
            agent_type: DEFAULT_WORKFLOW_SUBAGENT.to_string(),
            when_to_use: String::new(),
            when_to_use_lean: None,
            tools_description: String::new(),
        }]
    }

    async fn spawn(
        &self,
        request: SubagentSpawnRequest,
        _inherit: SubagentInheritance,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.started_prompts
            .lock()
            .unwrap()
            .push(request.prompt.clone());
        if let Some(tx) = self.started_tx.lock().unwrap().as_ref() {
            let _ = tx.send(request.prompt);
        }
        self.wait_until_released().await;
        Ok(completed_probe_result(protocol::AgentId::new()))
    }

    async fn spawn_workflow_with_observer(
        &self,
        request: SubagentSpawnRequest,
        _inherit: SubagentInheritance,
        _progress: Option<tokio::sync::mpsc::Sender<String>>,
        _observer: Option<Arc<dyn platform_api::subagent_spawn::SubagentSpawnObserver>>,
        _watchdog: platform_api::subagent_spawn::WorkflowQueryWatchdog,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.started
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.started_prompts
            .lock()
            .unwrap()
            .push(request.prompt.clone());
        if let Some(tx) = self.started_tx.lock().unwrap().as_ref() {
            let _ = tx.send(request.prompt);
        }
        self.wait_until_released().await;
        Ok(completed_probe_result(protocol::AgentId::new()))
    }
}

#[derive(Default)]
struct AllocationCountingObserver {
    allocations: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl platform_api::subagent_spawn::SubagentSpawnObserver for AllocationCountingObserver {
    async fn on_event(&self, event: platform_api::subagent_spawn::SubagentObservation) {
        if matches!(
            event,
            platform_api::subagent_spawn::SubagentObservation::Allocated { .. }
        ) {
            self.allocations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

#[async_trait]
impl SubagentSpawner for EchoSpawner {
    async fn agent_listing(&self) -> Vec<platform_api::subagent_spawn::SubagentListingEntry> {
        [
            "general-purpose",
            "Explore",
            "code-reviewer",
            "workflow-subagent",
        ]
        .iter()
        .map(|t| platform_api::subagent_spawn::SubagentListingEntry {
            agent_type: (*t).to_string(),
            when_to_use: String::new(),
            when_to_use_lean: None,
            tools_description: String::new(),
        })
        .collect()
    }
    async fn spawn(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        if let Some(totals) = &self.inherited_budget_totals {
            let total = inherit.budget.snapshot_total_nano_usd().await;
            totals.lock().unwrap().push(total);
        }
        self.seen.lock().unwrap().push(request.prompt.clone());
        self.seen_reqs.lock().unwrap().push(request.clone());
        if self.fail {
            return Ok(SubagentResult::Failed {
                agent_id: protocol::AgentId::new(),
                reason: "boom".into(),
                usage: SubagentUsage::default(),
            });
        }
        Ok(SubagentResult::Completed {
            agent_id: protocol::AgentId::new(),
            content: Value::String(format!("echo:{}", request.prompt)),
            usage: SubagentUsage {
                output_tokens: 100,
                ..Default::default()
            },
            total_tool_use_count: self.total_tool_use_count,
            total_duration_ms: self.total_duration_ms,
            total_tokens: self.total_tokens,
            assistant_message_count: 0,
            response_char_count: 0,
            last_request_id: None,
            cumulative_usage: SubagentUsage::default(),
            usage_complete: true,
        })
    }
}

#[derive(Default)]
struct RecordingWorktreeManager {
    created: StdMutex<Vec<(String, platform_api::worktree::WorktreeHandle)>>,
    removed: StdMutex<Vec<platform_api::worktree::WorktreeHandle>>,
}

impl RecordingWorktreeManager {
    fn created(&self) -> Vec<(String, platform_api::worktree::WorktreeHandle)> {
        self.created.lock().unwrap().clone()
    }
}

#[async_trait]
impl platform_api::worktree::WorktreeManager for RecordingWorktreeManager {
    async fn create_worktree(
        &self,
        slug: &str,
        _base_branch: Option<&str>,
        _copy_includes: &[PathBuf],
    ) -> Result<platform_api::worktree::WorktreeHandle, platform_api::worktree::WorktreeError> {
        let handle = platform_api::worktree::WorktreeHandle {
            path: PathBuf::from(format!("/tmp/mock-worktrees/{slug}")),
            branch_name: format!("worktree-{slug}"),
            base_commit: Some("base".into()),
        };
        self.created
            .lock()
            .unwrap()
            .push((slug.to_string(), handle.clone()));
        Ok(handle)
    }

    async fn remove_worktree(
        &self,
        handle: &platform_api::worktree::WorktreeHandle,
    ) -> Result<(), platform_api::worktree::WorktreeError> {
        self.removed.lock().unwrap().push(handle.clone());
        Ok(())
    }

    async fn list_worktrees(
        &self,
    ) -> Result<Vec<platform_api::worktree::WorktreeInfo>, platform_api::worktree::WorktreeError>
    {
        Ok(Vec::new())
    }

    async fn cleanup_stale(
        &self,
        _max_age: std::time::Duration,
    ) -> Result<Vec<PathBuf>, platform_api::worktree::WorktreeError> {
        Ok(Vec::new())
    }

    fn is_supported(&self) -> bool {
        true
    }
}

// ---- Inert ToolInvoker / BudgetEnforcerHandle ---------------------------

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

struct RecordingBudget {
    scoped: Arc<StdMutex<Vec<protocol::SessionId>>>,
    scoped_total: u64,
}

#[async_trait]
impl BudgetEnforcerHandle for RecordingBudget {
    async fn check_and_charge(&self, _nano_usd: u64) -> Result<(), BudgetError> {
        Ok(())
    }

    async fn snapshot_total_nano_usd(&self) -> u64 {
        0
    }

    fn scoped_for_session(
        &self,
        session_id: protocol::SessionId,
    ) -> Option<Arc<dyn BudgetEnforcerHandle>> {
        self.scoped.lock().unwrap().push(session_id);
        Some(Arc::new(SnapshotBudget(self.scoped_total)))
    }
}

struct SnapshotBudget(u64);

#[async_trait]
impl BudgetEnforcerHandle for SnapshotBudget {
    async fn check_and_charge(&self, _nano_usd: u64) -> Result<(), BudgetError> {
        Ok(())
    }

    async fn snapshot_total_nano_usd(&self) -> u64 {
        self.0
    }
}

struct BudgetInspectingFusionExecutor {
    inherited_budget_totals: Arc<StdMutex<Vec<u64>>>,
}

#[async_trait]
impl FusionExecutor for BudgetInspectingFusionExecutor {
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
                let total = inherit.budget().snapshot_total_nano_usd().await;
                this.inherited_budget_totals.lock().unwrap().push(total);
                Ok(workflow_fusion_result())
            },
        )
    }

    fn agent_surface(&self) -> FusionAgentSurface {
        FusionAgentSurface {
            enabled: true,
            ..FusionAgentSurface::default()
        }
    }
}

// ---- In-memory FileSystem (mirrors the other handler fixtures) ----------

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
        let total_lines = content.lines().count() as u64;
        Ok(FileContent {
            content: trimmed,
            truncated,
            total_lines,
        })
    }
    async fn write_file(&self, path: &str, body: &str) -> Result<(), FsError> {
        self.files
            .lock()
            .await
            .insert(path.to_string(), body.to_string());
        Ok(())
    }
    fn is_within_workspace(&self, _: &str) -> bool {
        true
    }
    async fn watch(
        &self,
        _: &str,
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
        if let Some(s) = map.get_mut(path) {
            s.truncate(usize::try_from(len).unwrap_or(usize::MAX));
        }
        Ok(())
    }
    async fn file_mtime(&self, _: &str) -> Result<std::time::SystemTime, FsError> {
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
    async fn symlink(&self, _: &str, _: &str) -> Result<(), FsError> {
        Ok(())
    }
    async fn flock_exclusive(&self, _: &str) -> Result<Box<dyn FlockGuard>, FsError> {
        Err(FsError::Io("not supported".into()))
    }
    async fn fsync(&self, _: &str) -> Result<(), FsError> {
        Ok(())
    }
}

// ---- Recording status sink ----------------------------------------------

#[derive(Default)]
struct RecordingSink {
    statuses: StdMutex<Vec<(String, TaskStatus)>>,
    workflow_outcome: StdMutex<Option<platform_api::task_registry::WorkflowTerminalOutcome>>,
    calls: StdMutex<Vec<&'static str>>,
}
#[async_trait]
impl TaskStatusSink for RecordingSink {
    async fn set_status(&self, task_id: &str, status: TaskStatus) {
        if status.is_terminal() {
            self.calls.lock().unwrap().push("status");
        }
        self.statuses
            .lock()
            .unwrap()
            .push((task_id.to_string(), status));
    }

    async fn set_workflow_outcome(
        &self,
        _task_id: &str,
        outcome: platform_api::task_registry::WorkflowTerminalOutcome,
    ) {
        self.calls.lock().unwrap().push("outcome");
        *self.workflow_outcome.lock().unwrap() = Some(outcome);
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
impl RecordingSink {
    fn last_status(&self) -> Option<TaskStatus> {
        self.statuses.lock().unwrap().last().map(|(_, s)| *s)
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }
}

struct BlockingWorkflowTerminalSink {
    inner: RecordingSink,
    started_tx: StdMutex<Option<oneshot::Sender<()>>>,
    release_rx: TokioMutex<Option<oneshot::Receiver<()>>>,
    terminalizing: StdMutex<StdHashSet<String>>,
}

impl BlockingWorkflowTerminalSink {
    fn new(started_tx: oneshot::Sender<()>, release_rx: oneshot::Receiver<()>) -> Self {
        Self {
            inner: RecordingSink::default(),
            started_tx: StdMutex::new(Some(started_tx)),
            release_rx: TokioMutex::new(Some(release_rx)),
            terminalizing: StdMutex::new(StdHashSet::new()),
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
impl TaskStatusSink for BlockingWorkflowTerminalSink {
    async fn set_status(&self, task_id: &str, status: TaskStatus) {
        self.inner.set_status(task_id, status).await;
    }

    async fn set_workflow_outcome(
        &self,
        task_id: &str,
        outcome: platform_api::task_registry::WorkflowTerminalOutcome,
    ) {
        self.inner.set_workflow_outcome(task_id, outcome).await;
    }

    async fn finish_workflow_terminal(
        &self,
        task_id: &str,
        outcome: platform_api::task_registry::WorkflowTerminalOutcome,
        status: TaskStatus,
    ) {
        self.terminalizing
            .lock()
            .unwrap()
            .insert(task_id.to_string());
        self.inner.set_workflow_outcome(task_id, outcome).await;
        if let Some(tx) = self.started_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
        if let Some(rx) = self.release_rx.lock().await.take() {
            let _ = rx.await;
        }
        self.inner.set_status(task_id, status).await;
        self.terminalizing.lock().unwrap().remove(task_id);
    }

    async fn is_terminal(&self, task_id: &str) -> bool {
        if self.terminalizing.lock().unwrap().contains(task_id) {
            return true;
        }
        self.inner.is_terminal(task_id).await
    }
}

#[derive(Default)]
struct RegistrationSink {
    statuses: StdMutex<Vec<(String, TaskStatus)>>,
    registered: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl TaskStatusSink for RegistrationSink {
    async fn set_status(&self, task_id: &str, status: TaskStatus) {
        self.statuses
            .lock()
            .unwrap()
            .push((task_id.to_string(), status));
    }

    async fn is_registered(&self, _task_id: &str) -> bool {
        self.registered.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl RegistrationSink {
    fn last_status(&self) -> Option<TaskStatus> {
        self.statuses.lock().unwrap().last().map(|(_, s)| *s)
    }

    fn statuses(&self) -> Vec<(String, TaskStatus)> {
        self.statuses.lock().unwrap().clone()
    }

    fn set_registered(&self, registered: bool) {
        self.registered
            .store(registered, std::sync::atomic::Ordering::Relaxed);
    }
}

#[derive(Default)]
struct TranscriptOverrideSpawner {
    overrides: StdMutex<Vec<Option<PathBuf>>>,
    retarget_marker: StdMutex<Vec<String>>,
    transcript_lines: bool,
}

#[async_trait]
impl SubagentSpawner for TranscriptOverrideSpawner {
    async fn agent_listing(&self) -> Vec<platform_api::subagent_spawn::SubagentListingEntry> {
        vec![platform_api::subagent_spawn::SubagentListingEntry {
            agent_type: DEFAULT_WORKFLOW_SUBAGENT.to_string(),
            when_to_use: String::new(),
            when_to_use_lean: None,
            tools_description: String::new(),
        }]
    }

    async fn spawn(
        &self,
        request: SubagentSpawnRequest,
        inherit: SubagentInheritance,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.spawn_with_progress(request, inherit, None).await
    }

    async fn spawn_with_progress(
        &self,
        _request: SubagentSpawnRequest,
        _inherit: SubagentInheritance,
        _progress: Option<tokio::sync::mpsc::Sender<String>>,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        let override_dir = agent::workflow_transcript_subdir_override();
        self.overrides.lock().unwrap().push(override_dir.clone());
        self.retarget_marker.lock().unwrap().push(format!(
            "seen:{}",
            override_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<none>".to_string())
        ));
        let agent_id = protocol::AgentId::new();
        if self.transcript_lines {
            let dir = override_dir.expect("workflow transcript override present");
            std::fs::create_dir_all(&dir).expect("create transcript dir");
            let path = dir.join(format!("agent-{agent_id}.jsonl"));
            std::fs::write(
                path,
                format!(
                    "{}\n",
                    serde_json::json!({
                        "message": protocol::ConversationMessage::Assistant {
                            id: protocol::MessageId::new(),
                            content: vec![protocol::ContentBlock::Text {
                                text: "hello from child".to_string(),
                            }],
                            stop_reason: None,
                        }
                    })
                ),
            )
            .expect("write transcript line");
        }
        Ok(SubagentResult::Completed {
            agent_id,
            content: Value::String("ok".to_string()),
            usage: SubagentUsage::default(),
            total_tool_use_count: 0,
            total_duration_ms: 0,
            total_tokens: 0,
            assistant_message_count: 0,
            response_char_count: 0,
            last_request_id: None,
            cumulative_usage: SubagentUsage::default(),
            usage_complete: true,
        })
    }
}

// ---- Helpers ------------------------------------------------------------

fn make_ctx(fs: Arc<dyn FileSystem>) -> TaskContext {
    TaskContext {
        fs,
        runtime: Arc::new(MockRuntimeSpawner::default()),
    }
}

fn workflow_input(script: &str) -> TaskSpawnInput {
    TaskSpawnInput::LocalWorkflow {
        session_uuid: None,
        workflow_id: "wf".into(),
        script: script.into(),
        resume_from_run_id: None,
        args: None,
        run_id: None,
        parent_model: None,
        parent_model_profile: None,
        invocation_mode: Some("inline".to_string()),
        workflow_source: Some("inline".to_string()),
        script_is_verbatim_builtin: Some(false),
        transcript_subdir: None,
        launched_from_subagent: false,
        tool_use_id: None,
        creator_teammate_name: None,
        creator_team_name: None,
        creator_agent_id: None,
        scope: None,
    }
}

/// Poll the sink until it reports a terminal status (the worker runs on the
/// `MockRuntimeSpawner`'s tokio task, so yields let it finish).
async fn await_terminal(sink: &Arc<RecordingSink>) -> TaskStatus {
    for _ in 0..400 {
        if let Some(s) = sink.last_status() {
            if s.is_terminal() {
                return s;
            }
        }
        tokio::task::yield_now().await;
    }
    sink.last_status().expect("worker never reported a status")
}

// ==== Bridge-level tests (run_workflow_script directly) ==================

fn logs(outcome: &workflow::RunOutcome) -> Vec<String> {
    outcome
        .progress
        .iter()
        .filter_map(|p| match p {
            workflow::Progress::Log { message: s } => Some(s.clone()),
            workflow::Progress::Phase { .. } | workflow::Progress::Agent { .. } => None,
        })
        .collect()
}

async fn run_bridge(script: &str, spawner: Arc<EchoSpawner>) -> workflow::RunOutcome {
    run_workflow_script(
        script,
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .expect("workflow runs to completion")
}

/// Reproduction for the "/workflows task stuck running" report: the production
/// path wires a progress channel (`Some(ptx)`) and joins the run future with a
/// `drain` that reads the channel until all senders drop — the exact pattern the
/// `spawn` worker uses (`tokio::join!(run, drain)`). The other bridge tests pass
/// `None` for the progress sender, so this path (and any sender that outlives the
/// run) was never exercised. Uses an ASYNC spawner that actually yields, closer
/// to the real engine dispatch than the synchronous `EchoSpawner`.
#[tokio::test]
async fn run_with_progress_drain_completes_and_does_not_hang() {
    struct YieldSpawner;
    #[async_trait]
    impl SubagentSpawner for YieldSpawner {
        async fn agent_listing(&self) -> Vec<platform_api::subagent_spawn::SubagentListingEntry> {
            Vec::new()
        }
        async fn spawn(
            &self,
            request: SubagentSpawnRequest,
            _inherit: SubagentInheritance,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            // Yield + a tiny sleep so the spawn genuinely awaits (real dispatch
            // suspends on the network), rather than returning synchronously.
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            Ok(SubagentResult::Completed {
                agent_id: protocol::AgentId::new(),
                content: Value::String(format!("echo:{}", request.prompt)),
                usage: SubagentUsage {
                    output_tokens: 1,
                    ..Default::default()
                },
                total_tool_use_count: 0,
                total_duration_ms: 0,
                total_tokens: 0,
                assistant_message_count: 0,
                response_char_count: 0,
                last_request_id: None,
                cumulative_usage: SubagentUsage::default(),
                usage_complete: true,
            })
        }
    }

    let (ptx, mut prx) = tokio::sync::mpsc::unbounded_channel::<String>();
    // The `spawn` worker's drainer: read progress lines until every sender drops.
    let drain = async move {
        let mut lines = 0usize;
        while prx.recv().await.is_some() {
            lines += 1;
        }
        lines
    };
    let script = "export const meta = { name: 'x', description: 'y', phases: [{ title: 'A' }, { title: 'B' }] };\n\
                  phase('A'); const a = await agent('p1'); phase('B'); const b = await agent('p2'); return { a, b };";
    let run = run_workflow_script(
        script,
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        Arc::new(YieldSpawner),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        Some(ptx),
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    );
    // If a progress sender outlives `run`, `drain` never ends and this join hangs.
    let joined = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(run, drain)
    })
    .await;
    assert!(
        joined.is_ok(),
        "workflow with a progress drain HUNG — join!(run, drain) never completed"
    );
    let (outcome, drained_lines) = joined.unwrap();
    assert!(outcome.is_ok(), "run failed: {:?}", outcome.err());
    // 2 phases + 2 agents (start+done each) → several progress lines drained.
    assert!(
        drained_lines >= 4,
        "expected progress lines, got {drained_lines}"
    );
}

/// The 1000-agent lifetime cap: the 1001st real `agent()` call rejects with the
/// byte-exact `WorkflowAgentCapError` message, terminating the run.
#[tokio::test]
async fn agent_cap_rejects_the_1001st_spawn() {
    let spawner = Arc::new(EchoSpawner::default());
    let result = run_workflow_script(
        "for (let i = 0; i < 1001; i++) { await agent('x'); } return 'done';",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await;
    let err = result.expect_err("the 1001st agent() must throw the cap error");
    let msg = format!("{err}");
    assert!(
        msg.contains("Workflow agent() call cap reached (1000)"),
        "got: {msg}"
    );
}

/// The lifetime cap is enforced per call even when all calls arrive in one
/// parallel batch: the first 1000 run and only later calls receive the cap
/// error sentinel, which `parallel()` converts to `null`.
#[tokio::test]
async fn agent_cap_admits_first_1000_parallel_calls() {
    let spawner = Arc::new(EchoSpawner::default());
    let outcome = run_workflow_script(
        "const rs = await parallel(Array.from({ length: 1001 }, () => () => agent('x'))); return rs.length;",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner.clone(),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .expect("parallel cap batch should resolve with null for the overflow slot");
    assert_eq!(outcome.result.as_deref(), Some("1001"));
    assert_eq!(spawner.seen.lock().unwrap().len(), 1000);
}

/// An explicit unknown `agentType` throws the byte-exact not-found error
/// listing the available agents; a known one runs fine. The workflow id here
/// (`"local-app-build"`) deliberately has NO `:` -- a saved/inline, non-plugin
/// workflow -- so this also pins that the plugin-namespace retry added for
/// P0-2 (`plugin_workflow_resolves_bare_agent_type_under_its_namespace`
/// below) does not change behavior for a non-plugin workflow: the error stays
/// byte-exact, with no "(also tried ...)" clause.
#[tokio::test]
async fn unknown_agent_type_throws_not_found() {
    let spawner = Arc::new(EchoSpawner::default());
    let result = run_workflow_script(
        "await agent('p', { agentType: 'nope' }); return 'done';",
        DEFAULT_WORKFLOW_SUBAGENT,
        "local-app-build",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await;
    let err = result.expect_err("unknown agentType must throw");
    let msg = format!("{err}");
    assert!(
        msg.contains(
            "agent({agentType}): agent type 'nope' not found. Available agents: general-purpose, Explore, code-reviewer"
        ),
        "got: {msg}"
    );
    assert!(
        !msg.contains("also tried"),
        "non-plugin (no ':' in workflow id) error must not gain the plugin-namespace \
         'also tried' clause: {msg}"
    );

    // r4-tests-honesty-07: the doc comment's second half -- "a known one runs
    // fine" -- was never exercised. Every assertion above is equally
    // satisfied by a resolver that rejects EVERY agent type, so on its own
    // this test cannot tell "unknown types are refused" from "nothing
    // resolves at all". Same workflow id, same spawner, same call shape;
    // only the agentType differs.
    let known_spawner = Arc::new(EchoSpawner::default());
    let outcome = run_workflow_script(
        "return await agent('p', { agentType: 'Explore' });",
        DEFAULT_WORKFLOW_SUBAGENT,
        "local-app-build",
        known_spawner.clone(),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .expect("a KNOWN agentType on the same workflow id must resolve, not throw");
    let reqs = known_spawner.seen_reqs.lock().unwrap();
    assert_eq!(
        reqs.len(),
        1,
        "the known agentType must actually reach the spawner -- asserting only on the returned \
         value would still pass if the call never dispatched"
    );
    assert_eq!(
        reqs[0].subagent_type, "Explore",
        "the resolved agent type must be the one the script asked for"
    );
    drop(reqs);
    assert!(
        outcome
            .result
            .as_deref()
            .is_some_and(|result| result.contains("echo:p")),
        "the known agent's result must come back to the script, got {:?}",
        outcome.result
    );
}

/// A minimal spawner whose agent listing is fully caller-controlled, so a test
/// can pin the EXACT set of registered agent types (e.g. only the
/// plugin-qualified name a real plugin loader would register).
#[derive(Default)]
struct NamespacedListingSpawner {
    listing: Vec<String>,
    seen_reqs: StdMutex<Vec<SubagentSpawnRequest>>,
}

#[async_trait]
impl SubagentSpawner for NamespacedListingSpawner {
    async fn agent_listing(&self) -> Vec<platform_api::subagent_spawn::SubagentListingEntry> {
        self.listing
            .iter()
            .map(|t| platform_api::subagent_spawn::SubagentListingEntry {
                agent_type: t.clone(),
                when_to_use: String::new(),
                when_to_use_lean: None,
                tools_description: String::new(),
            })
            .collect()
    }

    async fn spawn(
        &self,
        request: SubagentSpawnRequest,
        _inherit: SubagentInheritance,
    ) -> Result<SubagentResult, SubagentSpawnError> {
        self.seen_reqs.lock().unwrap().push(request.clone());
        Ok(SubagentResult::Completed {
            agent_id: protocol::AgentId::new(),
            content: Value::String(format!("echo:{}", request.prompt)),
            usage: SubagentUsage::default(),
            total_tool_use_count: 0,
            total_duration_ms: 0,
            total_tokens: 0,
            assistant_message_count: 0,
            response_char_count: 0,
            last_request_id: None,
            cumulative_usage: SubagentUsage::default(),
            usage_complete: true,
        })
    }
}

/// P0-2 positive case: the plugin loader registers agents as
/// `<plugin>:<agent>` (`plugin/src/manager.rs:1052-1055`), but a plugin's own
/// workflow script authors a BARE `agentType` -- it must not need to know
/// what namespace it was installed under. When the running workflow's own id
/// is plugin-qualified (`lingxi-local-app:local-app-build`), the dispatch
/// must resolve under that plugin's namespace and spawn with the QUALIFIED
/// subagent_type.
#[tokio::test]
async fn plugin_workflow_resolves_bare_agent_type_under_its_namespace() {
    let spawner = Arc::new(NamespacedListingSpawner {
        listing: vec!["lingxi-local-app:builder".to_string()],
        ..Default::default()
    });
    let outcome = run_workflow_script(
        "const r = await agent('p', { agentType: 'builder' }); return r;",
        DEFAULT_WORKFLOW_SUBAGENT,
        "lingxi-local-app:local-app-build",
        spawner.clone(),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .expect("a bare agentType matching the workflow's own plugin namespace must resolve");
    assert_eq!(outcome.result.as_deref(), Some("\"echo:p\""));
    let reqs = spawner.seen_reqs.lock().unwrap();
    assert_eq!(reqs.len(), 1, "exactly one agent() call must have spawned");
    assert_eq!(
        reqs[0].subagent_type, "lingxi-local-app:builder",
        "the spawn request must carry the NAMESPACE-QUALIFIED subagent_type, \
         not the script's bare 'builder'"
    );
}

/// r1-workflow-runtime-15, REVERSED. This test used to assert the opposite,
/// on the "closer scope wins" precedence `resolve_script_at` documents for
/// workflow NAME resolution (`tools/workflow/src/lib.rs:320-331`). That
/// precedent does not transfer: there the USER names the workflow, so their
/// own file winning is their intent. Here the name is written inside the
/// PLUGIN's own script, about the plugin's own component -- the user never
/// asked for a substitution. `builder`, `designer`, `operator`, `tester` and
/// `verifier` are all names a person plausibly gives a project agent, and
/// under bare-first any one of them silently replaced the corresponding
/// `lingxi-local-app:` agent for an entire local-app build, with no
/// diagnostic on any surface.
///
/// So: when the running workflow belongs to a plugin AND that plugin
/// registered `<plugin>:<agentType>`, the plugin's own agent is what runs.
/// The `agent_shadow` case below pins the other half -- a bare name the
/// plugin did NOT register still resolves normally, so this is a preference,
/// not a restriction to namespaced agents.
#[tokio::test]
async fn plugin_namespaced_agent_wins_over_a_bare_shadow_when_both_are_registered() {
    let spawner = Arc::new(NamespacedListingSpawner {
        listing: vec![
            "builder".to_string(),
            "lingxi-local-app:builder".to_string(),
        ],
        ..Default::default()
    });
    let outcome = run_workflow_script(
        "const r = await agent('p', { agentType: 'builder' }); return r;",
        DEFAULT_WORKFLOW_SUBAGENT,
        "lingxi-local-app:local-app-build",
        spawner.clone(),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .expect("the call must resolve, not throw");
    assert_eq!(outcome.result.as_deref(), Some("\"echo:p\""));
    let reqs = spawner.seen_reqs.lock().unwrap();
    assert_eq!(reqs.len(), 1, "exactly one agent() call must have spawned");
    assert_eq!(
        reqs[0].subagent_type, "lingxi-local-app:builder",
        "a project/user agent registered under the bare name must NOT shadow the plugin's own \
         namespaced agent of the same name inside that plugin's own workflow"
    );
}

/// The other half of the preference above, and the reason it is safe: a bare
/// agentType the plugin did NOT register still resolves to the bare listing
/// entry. Without this, "prefer the namespaced spelling" could have been
/// implemented as "require it", which would break every plugin script call to
/// a built-in agent such as `general-purpose`.
#[tokio::test]
async fn a_bare_agent_type_the_plugin_did_not_register_still_resolves() {
    let spawner = Arc::new(NamespacedListingSpawner {
        listing: vec![
            "general-purpose".to_string(),
            "lingxi-local-app:builder".to_string(),
        ],
        ..Default::default()
    });
    let outcome = run_workflow_script(
        "const r = await agent('p', { agentType: 'general-purpose' }); return r;",
        DEFAULT_WORKFLOW_SUBAGENT,
        "lingxi-local-app:local-app-build",
        spawner.clone(),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .expect("a bare agentType with no namespaced sibling must still resolve");
    assert_eq!(outcome.result.as_deref(), Some("\"echo:p\""));
    let reqs = spawner.seen_reqs.lock().unwrap();
    assert_eq!(reqs.len(), 1, "exactly one agent() call must have spawned");
    assert_eq!(
        reqs[0].subagent_type, "general-purpose",
        "the bare spelling must survive when the plugin registered no \
         `lingxi-local-app:general-purpose`"
    );
}

/// P0-2 negative case: even under a plugin-qualified workflow id, a name that
/// matches NEITHER `<plugin>:<name>` NOR the bare spelling still throws --
/// and the error must name BOTH spellings it tried, so a user debugging a
/// typo sees the actual namespace resolution that happened.
#[tokio::test]
async fn unknown_agent_type_under_plugin_workflow_names_both_spellings() {
    let spawner = Arc::new(NamespacedListingSpawner {
        listing: vec!["lingxi-local-app:builder".to_string()],
        ..Default::default()
    });
    let result = run_workflow_script(
        "await agent('p', { agentType: 'nope' }); return 'done';",
        DEFAULT_WORKFLOW_SUBAGENT,
        "lingxi-local-app:local-app-build",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await;
    let err = result.expect_err("an agentType matching neither spelling must still throw");
    let msg = format!("{err}");
    assert!(
        msg.contains("'nope'"),
        "error must name the bare spelling the script asked for: {msg}"
    );
    assert!(
        msg.contains("lingxi-local-app:nope"),
        "error must ALSO name the namespace-qualified spelling that was tried first: {msg}"
    );
}

/// A `NestedConfig` carrying a live plugin workflow registry with exactly the
/// given namespaced workflow names.
fn nested_with_plugin_workflows(names: &[&str]) -> NestedConfig {
    let registry = workflow::PluginWorkflowRegistry::new();
    registry.register(
        names
            .iter()
            .map(|name| workflow::PluginWorkflowEntry {
                name: (*name).to_string(),
                script_path: std::path::PathBuf::from(format!("/plugins/{name}.js")),
            })
            .collect(),
    );
    NestedConfig {
        plugin_workflows: Some(Arc::new(registry)),
        ..Default::default()
    }
}

/// P0-2, RESUME path. The Host relaunches a paused/adopted local-app build by
/// `script_path` with `name: None` (`harness-runtime/src/mobile/host.rs`), so the
/// run's `workflow_id` degrades to the script's BARE `meta.name`
/// (`"local-app-build"`, no `:`) -- the desktop host does the same by
/// preferring `meta.name` over `spec.name`. Splitting the id alone therefore
/// yields no namespace, and the very same three plugin scripts would throw
/// again on their first uncached `agent()` call after the journal goes live.
/// The namespace must instead be recovered from the plugin workflow registry:
/// exactly one plugin registers `lingxi-local-app:local-app-build`, so a bare
/// `builder` resolves under `lingxi-local-app`.
#[tokio::test]
async fn resumed_plugin_workflow_resolves_bare_agent_type_via_registry() {
    let spawner = Arc::new(NamespacedListingSpawner {
        listing: vec!["lingxi-local-app:builder".to_string()],
        ..Default::default()
    });
    let outcome = run_workflow_script(
        "const r = await agent('p', { agentType: 'builder' }); return r;",
        DEFAULT_WORKFLOW_SUBAGENT,
        // Bare, exactly as the resume/desktop paths derive it.
        "local-app-build",
        spawner.clone(),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        nested_with_plugin_workflows(&["lingxi-local-app:local-app-build"]),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .expect("a resumed plugin workflow must still resolve its own bare agentType");
    assert_eq!(outcome.result.as_deref(), Some("\"echo:p\""));
    let reqs = spawner.seen_reqs.lock().unwrap();
    assert_eq!(reqs.len(), 1, "exactly one agent() call must have spawned");
    assert_eq!(
        reqs[0].subagent_type, "lingxi-local-app:builder",
        "the namespace must be recovered from the plugin workflow registry when \
         the workflow id itself is bare"
    );
}

/// The registry fallback must not GUESS an owner. When two plugins each
/// register a workflow with the same bare name there is no unambiguous
/// namespace, so a bare `agentType` stays unresolved and the error keeps the
/// byte-exact non-plugin wording (no "(also tried ...)" clause).
#[tokio::test]
async fn ambiguous_bare_workflow_name_resolves_no_namespace() {
    let spawner = Arc::new(NamespacedListingSpawner {
        listing: vec!["lingxi-local-app:builder".to_string()],
        ..Default::default()
    });
    let result = run_workflow_script(
        "await agent('p', { agentType: 'builder' }); return 'done';",
        DEFAULT_WORKFLOW_SUBAGENT,
        "local-app-build",
        spawner.clone(),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        nested_with_plugin_workflows(&[
            "lingxi-local-app:local-app-build",
            "other-plugin:local-app-build",
        ]),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await;
    let err = result.expect_err("an ambiguous bare workflow name must not resolve a namespace");
    let msg = format!("{err}");
    assert!(
        msg.contains(
            "agent({agentType}): agent type 'builder' not found. Available agents: lingxi-local-app:builder"
        ),
        "got: {msg}"
    );
    assert!(
        !msg.contains("also tried"),
        "an ambiguous registry match must resolve NO namespace, so no second \
         spelling may be reported as tried: {msg}"
    );
    assert_eq!(
        spawner.seen_reqs.lock().unwrap().len(),
        0,
        "no agent may be spawned when the namespace is ambiguous"
    );
}

#[tokio::test]
async fn remote_isolation_is_rejected_in_local_workflow_build() {
    let result = run_workflow_script(
        "await agent('p', { isolation: 'remote' });",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        Arc::new(EchoSpawner::default()),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .expect_err("remote isolation must be unavailable in local build");
    assert!(
        format!("{result}").contains("agent({isolation:'remote'}) is not available in this build")
    );
}

/// A workflow that stays under the cap runs to completion unaffected.
#[tokio::test]
async fn under_cap_workflow_completes() {
    let spawner = Arc::new(EchoSpawner::default());
    let outcome = run_bridge(
        "for (let i = 0; i < 50; i++) { await agent('x'); } return 'ok';",
        spawner,
    )
    .await;
    assert_eq!(outcome.result.as_deref(), Some("\"ok\""));
}

/// `budget.spent()` is TURN-RELATIVE: the shared pool minus the turn-start
/// baseline (claude-code `getTurnSpent()=rT()-xtr`), so prior-turn output is
/// excluded.
#[tokio::test]
async fn budget_spent_is_turn_relative_via_baseline() {
    use std::sync::atomic::AtomicU64;
    // Pool already at 500 from prior turns; THIS turn started at 500 → the
    // baseline is 500, so a fresh 100-token subagent yields spent()==100.
    let pool = Arc::new(AtomicU64::new(500));
    let spawner = Arc::new(EchoSpawner::default());
    let outcome = run_workflow_script(
        "await agent('a'); log('spent=' + budget.spent()); return '';",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        Some(pool),
        500,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .expect("runs");
    assert!(
        logs(&outcome).iter().any(|l| l == "spent=100"),
        "spent should be turn-relative (100), logs: {:?}",
        logs(&outcome)
    );
}

/// The budget hard ceiling: once turn-relative spend reaches the target, the
/// next `agent()` throws the byte-exact `WorkflowBudgetExceededError`.
#[tokio::test]
async fn budget_ceiling_throws_when_turn_spend_exceeds_total() {
    use std::sync::atomic::AtomicU64;
    // total=100; pool already at 150 this turn (baseline 0) → 150 >= 100.
    let pool = Arc::new(AtomicU64::new(150));
    let spawner = Arc::new(EchoSpawner::default());
    let result = run_workflow_script(
        "await agent('a'); return 'done';",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        Some(100),
        Some(pool),
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await;
    let err = result.expect_err("over-budget agent() must throw");
    assert!(
        format!("{err}").contains("Workflow token budget exceeded (150 / 100 output tokens)"),
        "got: {err}"
    );
}

/// `budget.spent()` reads the shared pool: a pre-seeded value (standing in
/// for main-loop output the orchestrator already accumulated) plus every
/// subagent's output tokens — the union claude-code exposes, not own-spend.
#[tokio::test]
async fn shared_pool_makes_spent_read_main_loop_plus_subagents() {
    use std::sync::atomic::{AtomicU64, Ordering};
    // Pre-seed as if the main loop already spent 500 output tokens this turn.
    let pool = Arc::new(AtomicU64::new(500));
    let spawner = Arc::new(EchoSpawner::default()); // 100 output tokens/agent
    let outcome = run_workflow_script(
        "await agent('a'); await agent('b'); log('spent=' + budget.spent()); return '';",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        Some(1_000_000),
        Some(pool.clone()),
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .expect("workflow runs to completion");
    // 500 (seeded main loop) + 2×100 (the two subagents) = 700.
    assert_eq!(pool.load(Ordering::Relaxed), 700);
    assert!(
        logs(&outcome).iter().any(|l| l == "spent=700"),
        "spent() must read the shared pool live; got {:?}",
        logs(&outcome)
    );
}

/// Without a shared pool (the standalone/test path), `spent()` falls back to
/// a private counter of this run's own subagent output only.
#[tokio::test]
async fn no_shared_pool_falls_back_to_own_spend() {
    let spawner = Arc::new(EchoSpawner::default());
    let outcome = run_bridge(
        "await agent('a'); log('spent=' + budget.spent()); return '';",
        spawner,
    )
    .await;
    assert!(
        logs(&outcome).iter().any(|l| l == "spent=100"),
        "own-spend fallback should count just the one subagent; got {:?}",
        logs(&outcome)
    );
}

#[tokio::test]
async fn parallel_agents_round_trip_through_spawner_in_order() {
    let spawner = Arc::new(EchoSpawner::default());
    let script = r#"
        const rs = await parallel([
          () => agent('a'),
          () => agent('b'),
          () => agent('c'),
        ]);
        log('R:' + rs.join(','));
    "#;
    let outcome = run_bridge(script, spawner.clone()).await;
    assert_eq!(logs(&outcome), vec!["R:echo:a,echo:b,echo:c".to_string()]);
    let mut seen = spawner.seen.lock().unwrap().clone();
    seen.sort();
    assert_eq!(
        seen,
        vec!["a".to_string(), "b".to_string(), "c".to_string()]
    );
}

#[test]
fn make_request_maps_structured_output_parse_retries() {
    assert_eq!(
        make_request("designer", "p", "{}").structured_output_parse_retries,
        0
    );
    for retries in 0..=2 {
        let opts = serde_json::json!({"structuredOutputParseRetries": retries});
        assert_eq!(structured_output_parse_retries(&opts), Ok(retries));
        assert_eq!(
            make_request("designer", "p", &opts.to_string()).structured_output_parse_retries,
            retries
        );
    }
}

#[tokio::test]
async fn invalid_structured_output_parse_retries_reject_before_spawn() {
    for invalid in [
        serde_json::json!(-1),
        serde_json::json!(3),
        serde_json::json!(1.5),
        serde_json::json!("2"),
        serde_json::json!(null),
        serde_json::json!(true),
    ] {
        let spawner = Arc::new(EchoSpawner::default());
        let script = format!("await agent('p', {{ structuredOutputParseRetries: {invalid} }});");
        let error = run_workflow_script(
            &script,
            DEFAULT_WORKFLOW_SUBAGENT,
            "",
            spawner.clone(),
            Arc::new(MockInvoker),
            Arc::new(MockBudget),
            None,
            None,
            None,
            None,
            0,
            NestedConfig::default(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(AnalyticsBus::new()),
            None,
            None,
        )
        .await
        .expect_err("invalid retry count must fail");
        assert!(
            error
                .to_string()
                .contains("expected an integer from 0 to 2"),
            "{error}"
        );
        assert!(spawner.seen_reqs.lock().unwrap().is_empty());
    }
}

#[test]
fn make_request_maps_effort_opt() {
    // `agent({effort})` — a level string or an integer — is carried raw.
    assert_eq!(
        make_request("general-purpose", "p", r#"{"effort":"high"}"#).effort,
        Some(serde_json::json!("high"))
    );
    assert_eq!(
        make_request("general-purpose", "p", r#"{"effort":8000}"#).effort,
        Some(serde_json::json!(8000))
    );
    assert!(make_request("general-purpose", "p", "{}").effort.is_none());
}

#[test]
fn make_request_applies_workflow_stage_tool_denies() {
    let request = make_request(
        "general-purpose",
        "stage",
        r#"{"agentType":"builder","disallowedTools":["LocalAppGet","LocalAppScaffold","LocalAppRuntime"]}"#,
    );
    for tool in ["LocalAppGet", "LocalAppScaffold", "LocalAppRuntime"] {
        assert!(
            request
                .additional_disallowed_tools
                .contains(&tool.to_string()),
            "missing stage deny for {tool}"
        );
    }
    assert!(
        request
            .additional_disallowed_tools
            .contains(&"Workflow".to_string()),
        "workflow-subagent denies must still be unioned"
    );
}

#[test]
fn make_request_maps_provider_qualified_model_opts() {
    let request = make_request(
        "general-purpose",
        "p",
        r#"{"model":"deepseek-flash","modelProfile":"deepseek"}"#,
    );
    assert_eq!(request.model.as_deref(), Some("deepseek-flash"));
    assert_eq!(request.model_profile.as_deref(), Some("deepseek"));

    let snake_case = make_request(
        "general-purpose",
        "p",
        r#"{"model":"deepseek-flash","model_profile":"deepseek"}"#,
    );
    assert_eq!(snake_case.model_profile.as_deref(), Some("deepseek"));
}

#[tokio::test]
async fn workflow_agent_progress_keeps_provider_qualified_model() {
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
    run_workflow_script_with_live_updates(
        r#"await agent('design', {model:'deepseek-flash', modelProfile:'deepseek'});"#,
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        Arc::new(EchoSpawner::default()),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        Some(progress_tx),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
        None,
    )
    .await
    .expect("workflow runs to completion");
    let mut models = Vec::new();
    while let Ok(progress) = progress_rx.try_recv() {
        if progress.kind == "workflow_agent" {
            models.push(progress.model);
        }
    }
    assert_eq!(
        models,
        vec![
            Some("deepseek/deepseek-flash".to_string()),
            Some("deepseek/deepseek-flash".to_string()),
        ]
    );
}

#[tokio::test]
async fn workflow_emits_queued_progress_for_waiting_parallel_agents_before_slots_free() {
    let cap = concurrency_cap();
    let calls = (0..=cap)
        .map(|index| format!("() => agent('a{index}')"))
        .collect::<Vec<_>>()
        .join(",\n");
    let script = format!("const rs = await parallel([\n{calls}\n]);\nreturn rs.length;");
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let spawner = Arc::new(BlockingWorkflowObserverSpawner::new(started_tx));
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
    let run = tokio::spawn({
        let spawner = spawner.clone();
        async move {
            run_workflow_script_with_live_updates(
                &script,
                DEFAULT_WORKFLOW_SUBAGENT,
                "",
                spawner,
                Arc::new(MockInvoker),
                Arc::new(MockBudget),
                None,
                Some(progress_tx),
                None,
                None,
                None,
                None,
                0,
                NestedConfig::default(),
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
                Arc::new(AnalyticsBus::new()),
                None,
                None,
                None,
            )
            .await
        }
    });

    for index in 0..cap {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), started_rx.recv())
            .await
            .unwrap_or_else(|_| panic!("spawn {index} should start"))
            .expect("started prompt");
    }

    let queued = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut queued = Vec::new();
        while queued.len() < cap {
            let progress = progress_rx.recv().await.expect("queued progress");
            if progress.kind == "workflow_agent"
                && progress.state.as_deref() == Some("start")
                && progress
                    .tool_use_id
                    .as_deref()
                    .is_some_and(|tool_use_id| tool_use_id.ends_with("_queued"))
            {
                queued.push(progress);
            }
        }
        queued
    })
    .await
    .expect("queued events for admitted slots should arrive before a slot frees");

    assert_eq!(
        spawner.started_count(),
        cap,
        "the overflow agent should still be waiting on the concurrency cap"
    );
    assert_eq!(
        queued
            .iter()
            .map(|progress| progress.index)
            .collect::<Vec<_>>(),
        (0..cap as u64).collect::<Vec<_>>()
    );
    assert!(queued
        .iter()
        .all(|progress| progress.queued_at_ms.is_some() && progress.attempt == Some(1)));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), async {
            loop {
                let progress = progress_rx.recv().await.expect("progress channel open");
                if progress.kind == "workflow_agent"
                    && progress.state.as_deref() == Some("start")
                    && progress
                        .tool_use_id
                        .as_deref()
                        .is_some_and(|tool_use_id| tool_use_id.ends_with("_queued"))
                {
                    return progress;
                }
            }
        })
        .await
        .is_err(),
        "the overflow call must not emit queued until a buffered slot actually starts"
    );

    spawner.release_all();
    let outcome = run
        .await
        .expect("workflow task join")
        .expect("workflow succeeds");
    let expected_count = (cap + 1).to_string();
    assert_eq!(outcome.result.as_deref(), Some(expected_count.as_str()));
    assert_eq!(
        spawner.started_count(),
        cap + 1,
        "releasing the blocked calls should also let the overflow call run"
    );
}

#[tokio::test]
async fn workflow_rejected_agent_type_emits_no_queued_progress() {
    let spawner = Arc::new(EchoSpawner::default());
    let (progress_tx, mut progress_rx) = mpsc::unbounded_channel();
    let err = run_workflow_script_with_live_updates(
        "await agent('x', { agentType: 'missing-agent' }); return 'done';",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        Some(progress_tx),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
        None,
    )
    .await
    .expect_err("invalid agentType should throw before spawn");
    assert!(
        err.to_string()
            .contains("agent type 'missing-agent' not found"),
        "{err}"
    );

    while let Ok(progress) = progress_rx.try_recv() {
        assert!(
            !(progress.kind == "workflow_agent"
                && progress.state.as_deref() == Some("start")
                && progress
                    .tool_use_id
                    .as_deref()
                    .is_some_and(|tool_use_id| tool_use_id.ends_with("_queued"))),
            "rejected agent() calls must not emit queued progress: {progress:?}"
        );
    }
}

#[tokio::test]
async fn sequential_awaits_preserve_order() {
    let spawner = Arc::new(EchoSpawner::default());
    let script = r#"
        const a = await agent('1');
        const b = await agent('2');
        log(a + '|' + b);
    "#;
    let outcome = run_bridge(script, spawner.clone()).await;
    assert_eq!(logs(&outcome), vec!["echo:1|echo:2".to_string()]);
    assert_eq!(
        *spawner.seen.lock().unwrap(),
        vec!["1".to_string(), "2".to_string()]
    );
}

#[tokio::test]
async fn failed_agent_resolves_to_a_falsy_value() {
    let spawner = Arc::new(EchoSpawner {
        fail: true,
        ..Default::default()
    });
    let script = r#"
        const r = await agent('x');
        log('got:' + (r || 'NONE'));
    "#;
    let outcome = run_bridge(script, spawner).await;
    assert_eq!(logs(&outcome), vec!["got:NONE".to_string()]);
}

#[tokio::test]
async fn pipeline_stages_run_each_item_through_the_spawner() {
    let spawner = Arc::new(EchoSpawner::default());
    let script = r#"
        const rs = await pipeline(
          ['x', 'y'],
          (item) => agent(item),
          (prev) => agent(prev + '!'),
        );
        log('P:' + rs.join(','));
    "#;
    let outcome = run_bridge(script, spawner.clone()).await;
    assert_eq!(
        logs(&outcome),
        vec!["P:echo:echo:x!,echo:echo:y!".to_string()]
    );
}

#[tokio::test]
async fn agent_opts_map_to_the_spawn_request() {
    let spawner = Arc::new(EchoSpawner::default());
    let script = r#"
        await agent('p', { agentType: 'code-reviewer', model: 'opus', isolation: 'worktree', schema: { type: 'object' } });
        await agent('plain');
    "#;
    run_bridge(script, spawner.clone()).await;
    let reqs = spawner.seen_reqs.lock().unwrap().clone();
    assert_eq!(reqs.len(), 2);
    // agentType / model / isolation / schema opts → the spawn request.
    assert_eq!(reqs[0].subagent_type, "code-reviewer");
    assert_eq!(reqs[0].model.as_deref(), Some("opus"));
    assert_eq!(reqs[0].isolation.as_deref(), Some("worktree"));
    assert_eq!(reqs[0].schema.as_deref(), Some(r#"{"type":"object"}"#));
    // A bare agent(prompt) → default type (workflow-subagent), no overrides.
    assert_eq!(reqs[1].subagent_type, "workflow-subagent");
    assert_eq!(reqs[1].model, None);
    assert_eq!(reqs[1].isolation, None);
    assert_eq!(reqs[1].schema, None);
}

#[tokio::test]
async fn top_level_args_global_reaches_the_script() {
    let spawner = Arc::new(EchoSpawner::default());
    let outcome = run_workflow_script(
        "log('a=' + args.a); return args;",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig {
            allow_nested: false,
            args: Some(r#"{"a":5}"#.to_string()),
            fs: None,
            plugin_workflows: None,
            session_uuid: None,
        },
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(logs(&outcome), vec!["a=5".to_string()]);
    assert_eq!(outcome.result.as_deref(), Some(r#"{"a":5}"#));
}

#[tokio::test]
async fn workflow_runs_a_nested_scriptpath_inline_sharing_the_runtime() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    // The nested workflow itself spawns an agent and reads its own args —
    // proving it runs IN the parent's runtime (shared spawner + globals).
    fs.write_file(
        "/wf/child.js",
        "const x = await agent('child-task'); return { got: x, n: args.n };",
    )
    .await
    .unwrap();
    let spawner = Arc::new(EchoSpawner::default());
    let parent = r#"
        const r = await workflow({ scriptPath: '/wf/child.js' }, { n: 9 });
        log('got=' + r.got + ' n=' + r.n);
        return r;
    "#;
    let outcome = run_workflow_script(
        parent,
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner.clone(),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig {
            allow_nested: true,
            args: None,
            fs: Some(fs),
            plugin_workflows: None,
            session_uuid: None,
        },
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .unwrap();
    // The nested workflow's agent() went through the PARENT's spawner.
    assert_eq!(
        *spawner.seen.lock().unwrap(),
        vec!["child-task".to_string()]
    );
    // Its return value (incl. its own args) flowed back to the parent.
    assert_eq!(logs(&outcome), vec!["got=echo:child-task n=9".to_string()]);
    assert_eq!(
        outcome.result.as_deref(),
        Some(r#"{"got":"echo:child-task","n":9}"#)
    );
}

#[tokio::test]
async fn workflow_runs_a_nested_name_from_user_workflows_dir() {
    let _g = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let config_dir = tempdir().unwrap();
    let workflows_dir = config_dir.path().join("workflows");
    std::fs::create_dir_all(&workflows_dir).unwrap();
    std::fs::write(
        workflows_dir.join("user-child.js"),
        "return { source: 'user', n: args.n };",
    )
    .unwrap();
    let old_config_dir = std::env::var_os(branding::CONFIG_DIR_ENV);
    std::env::set_var(branding::CONFIG_DIR_ENV, config_dir.path());

    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let spawner = Arc::new(EchoSpawner::default());
    let parent = r#"
        const r = await workflow({ name: 'user-child' }, { n: 7 });
        log('source=' + r.source + ' n=' + r.n);
        return r;
    "#;
    let outcome = run_workflow_script(
        parent,
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner.clone(),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig {
            allow_nested: true,
            args: None,
            fs: Some(fs),
            plugin_workflows: None,
            session_uuid: None,
        },
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await;

    match old_config_dir {
        Some(v) => std::env::set_var(branding::CONFIG_DIR_ENV, v),
        None => std::env::remove_var(branding::CONFIG_DIR_ENV),
    }

    let outcome = outcome.unwrap();
    assert_eq!(logs(&outcome), vec!["source=user n=7".to_string()]);
    assert_eq!(
        outcome.result.as_deref(),
        Some(r#"{"source":"user","n":7}"#)
    );
}

/// §14 — a nested `workflow({name})` call resolves a plugin's declared
/// workflow through the SAME `workflow::PluginWorkflowRegistry` that
/// `plugin::PluginManager::load_plugin` materializes into, after the
/// project/user saved-workflow directories have missed (there is
/// deliberately no `.lingxi/workflows`/user-config-dir file here, isolating
/// the plugin-registry branch from the project/user branch already covered
/// by `workflow_runs_a_nested_name_from_user_workflows_dir`).
#[tokio::test]
async fn workflow_runs_a_nested_name_from_plugin_workflow_registry() {
    // Serializes with the CONFIG_DIR_ENV mutators above: this test does not
    // change the var itself, but `resolve_nested_script`'s project/user probe
    // (which must miss for this test to isolate the plugin branch) reads it.
    let _g = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let script_dir = tempdir().unwrap();
    let script_path = script_dir.path().join("deploy.js");
    std::fs::write(&script_path, "return { source: 'plugin', n: args.n };").unwrap();

    let registry = Arc::new(workflow::PluginWorkflowRegistry::new());
    registry.register(vec![workflow::PluginWorkflowEntry {
        name: "acme:deploy".to_string(),
        script_path: script_path.clone(),
    }]);

    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let spawner = Arc::new(EchoSpawner::default());
    let parent = r#"
        const r = await workflow({ name: 'acme:deploy' }, { n: 11 });
        log('source=' + r.source + ' n=' + r.n);
        return r;
    "#;
    let outcome = run_workflow_script(
        parent,
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner.clone(),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig {
            allow_nested: true,
            args: None,
            fs: Some(fs),
            plugin_workflows: Some(registry),
            session_uuid: None,
        },
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .unwrap();

    assert_eq!(logs(&outcome), vec!["source=plugin n=11".to_string()]);
    assert_eq!(
        outcome.result.as_deref(),
        Some(r#"{"source":"plugin","n":11}"#)
    );
}

// ==== Handler-level tests (full Task lifecycle) =========================

fn make_handler(
    spawner: Arc<dyn SubagentSpawner>,
    mgr: Arc<TaskOutputManager>,
    sink: Arc<dyn TaskStatusSink>,
) -> LocalWorkflowHandler {
    LocalWorkflowHandler::new(spawner, Arc::new(MockInvoker), Arc::new(MockBudget), mgr)
        .with_status_sink(sink)
}

#[tokio::test]
async fn workflow_process_owners_use_allocation_receipts_without_async_progress() {
    struct ReceiptSpawner;
    #[async_trait]
    impl SubagentSpawner for ReceiptSpawner {
        async fn spawn(
            &self,
            _: SubagentSpawnRequest,
            _: SubagentInheritance,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            panic!("the ownership wrapper must always provide an observer")
        }
        async fn spawn_with_observer(
            &self,
            _: SubagentSpawnRequest,
            _: SubagentInheritance,
            _: Option<mpsc::Sender<String>>,
            observer: Option<Arc<dyn platform_api::subagent_spawn::SubagentSpawnObserver>>,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            let id = protocol::AgentId::new();
            observer.expect("ungated process observer").on_allocated(
                &platform_api::subagent_spawn::SubagentObservation::Allocated {
                    agent_id: id,
                    agent_type: "general-purpose".into(),
                    name: None,
                    model: "mock".into(),
                    model_profile: None,
                    persistent: false,
                    initial_message_index: 0,
                    origin_session_id: None,
                },
            );
            // No on_event, journal entry or progress callback is emitted.
            Ok(SubagentResult::Killed { agent_id: id })
        }
    }
    use platform_api::task_registry::TaskRegistryHandle;
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(dir.path().to_path_buf(), fs.clone()));
    let sink = Arc::new(crate::registry_status_sink::RegistryStatusSink::new());
    let handler = Arc::new(make_handler(
        Arc::new(ReceiptSpawner),
        mgr.clone(),
        sink.clone(),
    ));
    let mut registry =
        crate::registry::TaskRegistry::new(Arc::new(MockRuntimeSpawner::default()), fs, mgr);
    registry.register_handler(TaskType::LocalWorkflow, handler);
    let registry = Arc::new(registry);
    sink.bind(registry.clone());
    let mut results = Vec::new();
    for _ in 0..2 {
        let task = registry
            .spawn(
                TaskType::LocalWorkflow,
                workflow_input("return await agent('ownership probe');"),
                "probe".into(),
            )
            .await
            .unwrap();
        let owners = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let ids =
                    TaskRegistryHandle::process_owners_for_task(registry.as_ref(), &task).await;
                if !ids.is_empty() {
                    break ids;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("allocation receipt reaches the workflow record");
        assert_eq!(owners.len(), 1);
        assert_ne!(
            owners[0], task,
            "owner identity comes from the child pool, not the workflow id"
        );
        results.push(owners);
    }
    assert_ne!(
        results[0], results[1],
        "separate workflows cannot claim each other's processes"
    );
}

#[tokio::test]
async fn local_workflow_scopes_budget_to_its_origin_session_at_spawn() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let dir = tempfile::tempdir().unwrap();
    let output_manager = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let scoped = Arc::new(StdMutex::new(Vec::new()));
    let agent_budget_totals = Arc::new(StdMutex::new(Vec::new()));
    let fusion_budget_totals = Arc::new(StdMutex::new(Vec::new()));
    const SCOPED_TOTAL: u64 = 42_424;
    let budget: Arc<dyn BudgetEnforcerHandle> = Arc::new(RecordingBudget {
        scoped: scoped.clone(),
        scoped_total: SCOPED_TOTAL,
    });
    let spawner = Arc::new(EchoSpawner {
        inherited_budget_totals: Some(agent_budget_totals.clone()),
        ..Default::default()
    });
    let status_sink = Arc::new(RecordingSink::default());
    let handler = LocalWorkflowHandler::new(
        spawner.clone(),
        Arc::new(MockInvoker),
        budget,
        output_manager,
    )
    .with_status_sink(status_sink.clone())
    .with_fusion(Arc::new(BudgetInspectingFusionExecutor {
        inherited_budget_totals: fusion_budget_totals.clone(),
    }));
    let expected = protocol::SessionId::parse_prefixed("11111111-2222-4333-8444-555555555555")
        .expect("valid session id");
    let mut input = workflow_input(
        "const a = await agent('probe'); const f = await fusion('probe'); return { a, f };",
    );
    let TaskSpawnInput::LocalWorkflow {
        session_uuid,
        parent_model,
        parent_model_profile,
        ..
    } = &mut input
    else {
        unreachable!("workflow_input builds a LocalWorkflow request");
    };
    *session_uuid = Some(expected.as_uuid().to_string());
    *parent_model = Some("gpt-5.4".into());
    *parent_model_profile = Some("openai".into());

    let handle = handler
        .spawn(input, make_ctx(fs))
        .await
        .expect("workflow spawn succeeds");
    assert_eq!(*scoped.lock().unwrap(), vec![expected]);

    assert_eq!(await_terminal(&status_sink).await, TaskStatus::Completed);
    assert_eq!(
        *agent_budget_totals.lock().unwrap(),
        vec![SCOPED_TOTAL],
        "agent() must inherit the scoped handle returned at workflow spawn"
    );
    assert_eq!(
        spawner
            .seen_reqs
            .lock()
            .unwrap()
            .first()
            .and_then(|request| request.origin_session_id),
        Some(expected),
        "workflow agent() children must carry the trusted origin session for their own descendants"
    );
    assert_eq!(
        *fusion_budget_totals.lock().unwrap(),
        vec![SCOPED_TOTAL],
        "fusion() must inherit the same scoped handle for the workflow lifetime"
    );
    assert!(!handle.task_id.is_empty());
}

#[tokio::test]
async fn workflow_fusion_round_trips_a_compact_result() {
    let executor = ImmediateFusionExecutor::new(
        FusionAgentSurface {
            enabled: true,
            default_partial_ok: true,
            ..FusionAgentSurface::default()
        },
        3,
        Ok(workflow_fusion_result()),
    );
    let outcome = run_workflow_script_with_live_updates_and_fusion(
        "return await fusion('review this', { preset: 'fast', maxPanel: 4, partialOk: false });",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        Arc::new(EchoSpawner::default()),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        None,
        None,
        0,
        NestedConfig {
            session_uuid: Some("11111111-2222-4333-8444-555555555555".into()),
            ..Default::default()
        },
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        CancellationToken::new(),
        Some(executor.clone()),
        Some("wf_fusion".into()),
        Some("gpt-5.4".into()),
        Some("openai".into()),
        None,
        Arc::new(AnalyticsBus::new()),
        None,
        None,
        None,
    )
    .await
    .expect("workflow with fusion");

    let value: serde_json::Value =
        serde_json::from_str(outcome.result.as_deref().expect("result")).expect("json");
    let returned_run_id = value["run_id"]
        .as_str()
        .expect("fusion result must carry its trusted run id");
    let parsed_run_id = platform_api::FusionRunId::parse(returned_run_id)
        .expect("prepared Fusion must return a canonical trusted run id");
    assert_eq!(parsed_run_id.to_string(), returned_run_id);
    assert_ne!(returned_run_id, "fu_test");
    // Exact key-set assertion, not `.get("report").is_none()` — `PanelOutcome`
    // (platform-api/src/fusion.rs) never had a `report`/`candidate_answer`
    // field to begin with, so those two checks passed vacuously regardless
    // of whether the wire shape stayed compact. Pin the actual serialized
    // keys so adding a raw-report field back to `PanelOutcome` fails HERE.
    let panel_keys: std::collections::BTreeSet<&str> = value["panels"][0]
        .as_object()
        .expect("panels[0] is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        panel_keys,
        std::collections::BTreeSet::from(["panel_id", "status", "duration_ms", "usage"]),
        "fusion() must resolve with PanelOutcome's compact shape, never the raw PanelReport"
    );
    let seen = executor.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].origin, platform_api::FusionOrigin::Workflow);
    assert_eq!(
        executor.seen_sessions.lock().unwrap().as_slice(),
        &[protocol::SessionId::parse_prefixed(
            "11111111-2222-4333-8444-555555555555"
        )],
        "a background workflow stays tied to the session that launched it"
    );
    assert_eq!(seen[0].workflow_run_id.as_deref(), Some("wf_fusion"));
    assert_eq!(seen[0].parent_model, "gpt-5.4");
    assert_eq!(seen[0].parent_profile, "openai");
    assert_eq!(seen[0].preset, platform_api::FusionPreset::Fast);
    assert_eq!(seen[0].max_panel, Some(4));
    assert!(!seen[0].partial_ok);
}

#[tokio::test]
async fn workflow_fusion_resume_hits_the_journal_and_never_calls_the_executor() {
    // A resumed run pre-loads `journal` from the prior run's
    // `journal.jsonl` (`WorkflowJournalWriter::load_results`) before the
    // worker starts — this test skips straight to that loaded state rather
    // than round-tripping through the filesystem.
    let executor = ImmediateFusionExecutor::new(
        FusionAgentSurface {
            enabled: true,
            ..FusionAgentSurface::default()
        },
        3,
        Ok(workflow_fusion_result()),
    );
    let cached_result = serde_json::to_string(&workflow_fusion_result()).unwrap();
    let key = fusion_chain_key(
        "",
        "review this",
        &normalize_fusion_opts_for_chain_key("{}"),
    );
    let journal = Arc::new(StdMutex::new(HashMap::from([(key, cached_result.clone())])));
    let outcome = run_workflow_script_with_live_updates_and_fusion(
        "return await fusion('review this');",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        Arc::new(EchoSpawner::default()),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        Some(journal),
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        CancellationToken::new(),
        Some(executor.clone()),
        Some("wf_fusion".into()),
        Some("gpt-5.4".into()),
        Some("openai".into()),
        None,
        Arc::new(AnalyticsBus::new()),
        None,
        None,
        None,
    )
    .await
    .expect("resumed fusion() call replays from the journal");
    assert_eq!(outcome.result.as_deref(), Some(cached_result.as_str()));
    assert!(
        executor.seen.lock().unwrap().is_empty(),
        "a journal-cache hit must never dispatch to the executor"
    );
}

/// [R4-12] A journaled fusion() result must still replay on resume when the
/// LIVE executor's state has changed enough that a FRESHLY-PARSED request
/// would now be rejected before ever reaching `run()` — e.g. the user
/// edited `fusion.*` settings between the run that journaled this key and
/// the resume, and `preflight_error()` now returns
/// `Some(InvalidConfiguration)`. R3-16 fixed only the sibling half of this
/// (a REFUSAL still advances the resume cursor); the cache lookup itself
/// must not sit behind `parse_workflow_fusion_request`'s live
/// re-derivation, since replaying an already-journaled string needs no
/// executor at all. Judged by REPLAY, not by inspecting `preflight_error()`
/// — the executor's own call counter must stay at 0.
#[tokio::test]
async fn workflow_fusion_resume_replays_the_journal_even_when_the_live_executor_would_now_reject_a_fresh_request(
) {
    let executor = Arc::new(PreflightRejectedFusionExecutor {
        error: FusionError::InvalidConfiguration(
            "fusion.totalTimeoutMs exceeds the sum of its stage timeouts".into(),
        ),
        ..Default::default()
    });
    let cached_result = serde_json::to_string(&workflow_fusion_result()).unwrap();
    let key = fusion_chain_key(
        "",
        "review this",
        &normalize_fusion_opts_for_chain_key("{}"),
    );
    let journal = Arc::new(StdMutex::new(HashMap::from([(key, cached_result.clone())])));
    let outcome = run_workflow_script_with_live_updates_and_fusion(
        "return await fusion('review this');",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        Arc::new(EchoSpawner::default()),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        Some(journal),
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        CancellationToken::new(),
        Some(executor.clone()),
        Some("wf_fusion".into()),
        Some("gpt-5.4".into()),
        Some("openai".into()),
        None,
        Arc::new(AnalyticsBus::new()),
        None,
        None,
        None,
    )
    .await
    .expect(
        "a journaled fusion() result must replay even though a fresh \
         request would be preflight-rejected",
    );
    assert_eq!(outcome.result.as_deref(), Some(cached_result.as_str()));
    assert_eq!(
        executor.calls.load(AtomicOrdering::SeqCst),
        0,
        "a journal-cache hit must never call the executor, even one whose \
         preflight_error() would reject a freshly-parsed request"
    );
}

/// [Item 16 rework] A fusion() call rejected by the per-workflow cap must
/// still advance the SAME `running_key` resume cursor a served fusion() call
/// advances — exactly like the agent() arm's Phase A, which advances
/// unconditionally before its own cap/budget gates run in Phase B
/// (local_workflow.rs:~2801 vs ~2854). Otherwise every subsequently
/// journaled call chained off the fusion key misses on resume and re-spawns
/// instead of hitting the journal that exists precisely to prevent that.
///
/// Run 1 (fresh): `fusion('one')` and `fusion('two')` both really run (cap
/// 2), chaining `running_key` through F1 then F2, then `agent('three')`
/// chains off F2 into key A — all three journaled to disk. Run 2 (resume,
/// cap 1): `fusion('one')` hits the journal (seen 0 < cap 1, consumes the
/// only cap slot); `fusion('two')` is now cap-rejected (seen 1 >= cap 1)
/// without ever reaching `parse_workflow_fusion_request` or the executor.
/// The script catches that (the documented `WorkflowFusionCapError` idiom)
/// and falls through to `agent('three')`, which must still replay from the
/// journal at key A — reachable only if the cap-rejected `fusion('two')`
/// still advanced `running_key` from F1 to F2.
#[tokio::test]
async fn workflow_fusion_cap_rejection_still_advances_the_resume_cursor_for_later_calls() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let script = r#"
        await fusion('one');
        let caught = false;
        try { await fusion('two'); } catch (e) { caught = true; }
        const b = await agent('three');
        log('caught:' + caught + ' b:' + b);
        return {};
    "#;

    // Run 1: fresh run, cap 2 — both fusion calls really run, then agent()
    // chains off the second fusion key.
    let spawner1 = Arc::new(EchoSpawner::default());
    let sink1 = Arc::new(RecordingSink::default());
    let executor1 = ImmediateFusionExecutor::new(
        FusionAgentSurface {
            enabled: true,
            ..FusionAgentSurface::default()
        },
        2,
        Ok(workflow_fusion_result()),
    );
    let handler1 = LocalWorkflowHandler::new(
        spawner1.clone(),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        mgr.clone(),
    )
    .with_status_sink(sink1.clone())
    .with_fusion(executor1.clone());
    let mut input1 = workflow_input(script);
    if let TaskSpawnInput::LocalWorkflow {
        parent_model,
        parent_model_profile,
        ..
    } = &mut input1
    {
        *parent_model = Some("gpt-5.4".into());
        *parent_model_profile = Some("openai".into());
    }
    let handle1 = handler1.spawn(input1, make_ctx(fs.clone())).await.unwrap();
    assert_eq!(await_terminal(&sink1).await, TaskStatus::Completed);
    assert_eq!(
        executor1.seen.lock().unwrap().len(),
        2,
        "run 1: both fusion calls really run"
    );
    assert_eq!(
        spawner1.seen.lock().unwrap().len(),
        1,
        "run 1: agent('three') really spawns"
    );

    let spool1 = dir.path().join(format!("{}.output", handle1.task_id));
    let out1 = mgr
        .read(&spool1, crate::output_manager::OutputOptions::default())
        .await
        .unwrap();
    let run_id = out1
        .content
        .lines()
        .find_map(|l| l.strip_prefix("runId: "))
        .expect("runId surfaced")
        .to_string();
    assert!(
        out1.content.contains("caught:false"),
        "run 1's second fusion call must succeed (cap 2): {}",
        out1.content
    );

    // Run 2: resume with cap 1 — fusion('one') consumes the only cap slot
    // (from the journal, no executor call), fusion('two') is cap-rejected,
    // and agent('three') must still replay from the journal.
    let spawner2 = Arc::new(EchoSpawner::default());
    let sink2 = Arc::new(RecordingSink::default());
    let executor2 = ImmediateFusionExecutor::new(
        FusionAgentSurface {
            enabled: true,
            ..FusionAgentSurface::default()
        },
        1,
        Ok(workflow_fusion_result()),
    );
    let handler2 = LocalWorkflowHandler::new(
        spawner2.clone(),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        mgr.clone(),
    )
    .with_status_sink(sink2.clone())
    .with_fusion(executor2.clone());
    let input2 = TaskSpawnInput::LocalWorkflow {
        session_uuid: None,
        workflow_id: "wf".into(),
        script: script.into(),
        resume_from_run_id: Some(run_id),
        args: None,
        run_id: None,
        parent_model: Some("gpt-5.4".into()),
        parent_model_profile: Some("openai".into()),
        invocation_mode: Some("inline".to_string()),
        workflow_source: Some("inline".to_string()),
        script_is_verbatim_builtin: Some(false),
        transcript_subdir: None,
        launched_from_subagent: false,
        tool_use_id: None,
        creator_teammate_name: None,
        creator_team_name: None,
        creator_agent_id: None,
        scope: None,
    };
    let handle2 = handler2.spawn(input2, make_ctx(fs.clone())).await.unwrap();
    assert_eq!(await_terminal(&sink2).await, TaskStatus::Completed);

    assert!(
        executor2.seen.lock().unwrap().is_empty(),
        "neither fusion() call should reach the executor on resume: the \
first hits the journal, the second is cap-rejected"
    );
    let spool2 = dir.path().join(format!("{}.output", handle2.task_id));
    let out2 = mgr
        .read(&spool2, crate::output_manager::OutputOptions::default())
        .await
        .unwrap();
    assert!(
        out2.content.contains("caught:true"),
        "run 2's second fusion call must be cap-rejected (cap 1): {}",
        out2.content
    );
    assert!(
        spawner2.seen.lock().unwrap().is_empty(),
        "agent('three') must still replay from the journal after a \
cap-rejected fusion() call advanced the resume cursor — got a live spawn \
instead: {:?}",
        spawner2.seen.lock().unwrap()
    );
}

#[tokio::test]
async fn workflow_fusion_rejects_unknown_fields_before_executor_runs() {
    let executor = ImmediateFusionExecutor::new(
        FusionAgentSurface {
            enabled: true,
            ..FusionAgentSurface::default()
        },
        3,
        Ok(workflow_fusion_result()),
    );
    let err = run_workflow_script_with_live_updates_and_fusion(
        "await fusion('review this', { nope: true });",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        Arc::new(EchoSpawner::default()),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        CancellationToken::new(),
        Some(executor.clone()),
        Some("wf_fusion".into()),
        Some("gpt-5.4".into()),
        Some("openai".into()),
        None,
        Arc::new(AnalyticsBus::new()),
        None,
        None,
        None,
    )
    .await
    .expect_err("unknown field must reject");
    assert!(err.to_string().contains("unknown field"));
    // The `workflow/src/lib.rs` prelude's `__wf_pump` looks for this exact
    // marker (anywhere in the message — `FusionError::InvalidRequest`'s
    // Display prepends "invalid fusion request: ") to set
    // `err.name = "WorkflowFusionOptionError"`; keep the two byte-locked
    // together.
    assert!(
        err.to_string()
            .contains("Workflow fusion() received an unknown option"),
        "{err}"
    );
    assert!(executor.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn workflow_fusion_rejects_when_disabled_without_executor_calls() {
    let executor = ImmediateFusionExecutor::new(
        FusionAgentSurface::default(),
        3,
        Ok(workflow_fusion_result()),
    );
    let err = run_workflow_script_with_live_updates_and_fusion(
        "await fusion('review this');",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        Arc::new(EchoSpawner::default()),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        CancellationToken::new(),
        Some(executor.clone()),
        Some("wf_fusion".into()),
        Some("gpt-5.4".into()),
        Some("openai".into()),
        None,
        Arc::new(AnalyticsBus::new()),
        None,
        None,
        None,
    )
    .await
    .expect_err("disabled fusion must reject");
    assert!(err.to_string().contains("fusion is disabled"));
    assert!(executor.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn workflow_fusion_surfaces_cross_provider_denials_without_silently_downgrading() {
    let executor = ImmediateFusionExecutor::new(
        FusionAgentSurface {
            enabled: true,
            ..FusionAgentSurface::default()
        },
        3,
        Err(FusionError::CrossProviderDenied),
    );
    let err = run_workflow_script_with_live_updates_and_fusion(
        "await fusion('review this', { crossProvider: true });",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        Arc::new(EchoSpawner::default()),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        CancellationToken::new(),
        Some(executor.clone()),
        Some("wf_fusion".into()),
        Some("gpt-5.4".into()),
        Some("openai".into()),
        None,
        Arc::new(AnalyticsBus::new()),
        None,
        None,
        None,
    )
    .await
    .expect_err("cross-provider deny must reject");
    assert!(err
        .to_string()
        .contains("cross-provider fusion is not allowed"));
    let seen = executor.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].cross_provider);
}

#[tokio::test]
async fn workflow_fusion_refuses_when_the_turn_token_budget_is_already_spent() {
    // Mirrors the agent() batch's budget ceiling: a workflow that has already
    // spent its turn budget must not be able to launch a fusion() run (4-5x
    // the cost of a single agent() call) just because it queues on a
    // different channel.
    let executor = ImmediateFusionExecutor::new(
        FusionAgentSurface {
            enabled: true,
            ..FusionAgentSurface::default()
        },
        3,
        Ok(workflow_fusion_result()),
    );
    let shared_pool = Arc::new(AtomicU64::new(500));
    let err = run_workflow_script_with_live_updates_and_fusion(
        "await fusion('review this');",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        Arc::new(EchoSpawner::default()),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        Some(500),
        Some(shared_pool.clone()),
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        CancellationToken::new(),
        Some(executor.clone()),
        Some("wf_fusion".into()),
        Some("gpt-5.4".into()),
        Some("openai".into()),
        None,
        Arc::new(AnalyticsBus::new()),
        None,
        None,
        None,
    )
    .await
    .expect_err("budget-exhausted fusion() call must reject");
    assert!(
        err.to_string()
            .contains("Workflow token budget exceeded (500 / 500 output tokens)"),
        "{err}"
    );
    assert!(
        executor.seen.lock().unwrap().is_empty(),
        "the executor must never be called once the turn budget is spent"
    );
    // The budget snapshot itself is untouched by the refusal — nothing was
    // spent, so nothing should be added to it.
    assert_eq!(shared_pool.load(AtomicOrdering::SeqCst), 500);
}

#[tokio::test]
async fn workflow_fusion_adds_its_output_tokens_to_the_shared_spent_pool() {
    let executor = ImmediateFusionExecutor::new(
        FusionAgentSurface {
            enabled: true,
            ..FusionAgentSurface::default()
        },
        3,
        Ok(workflow_fusion_result_with_output_tokens(777)),
    );
    let shared_pool = Arc::new(AtomicU64::new(0));
    let outcome = run_workflow_script_with_live_updates_and_fusion(
        "return await fusion('review this');",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        Arc::new(EchoSpawner::default()),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        Some(10_000),
        Some(shared_pool.clone()),
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        CancellationToken::new(),
        Some(executor.clone()),
        Some("wf_fusion".into()),
        Some("gpt-5.4".into()),
        Some("openai".into()),
        None,
        Arc::new(AnalyticsBus::new()),
        None,
        None,
        None,
    )
    .await
    .expect("fusion budget check passes with headroom");
    assert!(outcome.result.is_some());
    assert_eq!(shared_pool.load(AtomicOrdering::SeqCst), 777);
}

#[tokio::test]
async fn workflow_fusion_enforces_the_per_workflow_call_cap() {
    let executor = ImmediateFusionExecutor::new(
        FusionAgentSurface {
            enabled: true,
            ..FusionAgentSurface::default()
        },
        1,
        Ok(workflow_fusion_result()),
    );
    let err = run_workflow_script_with_live_updates_and_fusion(
        "await fusion('one'); await fusion('two');",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        Arc::new(EchoSpawner::default()),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        CancellationToken::new(),
        Some(executor.clone()),
        Some("wf_fusion".into()),
        Some("gpt-5.4".into()),
        Some("openai".into()),
        None,
        Arc::new(AnalyticsBus::new()),
        None,
        None,
        None,
    )
    .await
    .expect_err("second fusion call must be rejected");
    assert!(err
        .to_string()
        .contains("Workflow fusion() call cap reached (1)"));
    assert_eq!(executor.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn workflow_fusion_budget_refusal_does_not_consume_a_call_cap_slot() {
    // Parity with the agent() batch arm (line ~2705 above), which increments
    // `metrics.call_count` only AFTER the budget-ceiling check passes: a
    // fusion() call refused for budget must not burn one of the per-workflow
    // cap slots, or a workflow that keeps retrying after a budget refusal
    // would exhaust its cap on refusals alone and never see a real cap
    // failure message once budget is available again. Cap is 1 here; both
    // calls are made while the turn budget is already fully spent, so BOTH
    // must fail with the budget message — if the first refusal wrongly
    // consumed the cap slot, the second call would fail with the cap
    // message instead.
    let executor = ImmediateFusionExecutor::new(
        FusionAgentSurface {
            enabled: true,
            ..FusionAgentSurface::default()
        },
        1,
        Ok(workflow_fusion_result()),
    );
    let shared_pool = Arc::new(AtomicU64::new(500));
    let outcome = run_workflow_script_with_live_updates_and_fusion(
        r#"
let e1 = null;
try { await fusion('one'); } catch (e) { e1 = e.message; }
let e2 = null;
try { await fusion('two'); } catch (e) { e2 = e.message; }
return { e1, e2 };
"#,
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        Arc::new(EchoSpawner::default()),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        Some(500),
        Some(shared_pool.clone()),
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        CancellationToken::new(),
        Some(executor.clone()),
        Some("wf_fusion".into()),
        Some("gpt-5.4".into()),
        Some("openai".into()),
        None,
        Arc::new(AnalyticsBus::new()),
        None,
        None,
        None,
    )
    .await
    .expect("script itself does not throw — both calls are caught");
    let result = outcome.result.expect("script returns {e1, e2}");
    assert!(
        result.contains("Workflow token budget exceeded (500 / 500 output tokens)")
            && result.matches("Workflow token budget exceeded").count() == 2,
        "both calls must fail with the budget message, not a cap message: {result}"
    );
    assert!(
        !result.contains("call cap reached"),
        "the first (budget-refused) call must not have consumed a cap slot: {result}"
    );
    assert!(
        executor.seen.lock().unwrap().is_empty(),
        "the executor must never be called once the turn budget is spent"
    );
}

// NOTE (G012, WP7 fix round 1): a
// `workflow_fusion_run_inherits_the_workflow_transcript_subdir_override` test
// stood here, asserting that the fusion() arm's
// `agent::with_transcript_subdir_override` wrap makes Fusion panels inherit
// this workflow run's transcript subdir the way agent() subagents do. It was
// removed: the reviewer proved by instrumenting `ImmediateFusionExecutor::run`
// to read the override from inside a `JoinSet::spawn`ed task (the shape
// `fusion::panel::run_panels` actually uses to spawn each panel) that the
// override never crosses that spawn boundary — `tokio::task_local!` scopes
// do not survive `tokio::spawn`/`JoinSet::spawn`. The test only went green
// because `ImmediateFusionExecutor::run` read the task-local on the caller's
// own task, upstream of where production loses it. The production wrap was
// removed from local_workflow.rs's fusion arm for the same reason (see the
// comment there); this is an open cross-lane residual — the real fix touches
// `fusion/src/panel.rs` (lane B / WP2b) and `agent/src/handle.rs`'s
// `build_subagent_context` (lane A / WP2a), neither owned by this package.

#[tokio::test]
async fn workflow_kill_cancels_an_inflight_fusion() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let sink = Arc::new(RecordingSink::default());
    let (started_tx, started_rx) = oneshot::channel();
    let (cancelled_tx, cancelled_rx) = oneshot::channel();
    let executor = BlockingFusionExecutor::new(started_tx, cancelled_tx);
    let handler = LocalWorkflowHandler::new(
        Arc::new(EchoSpawner::default()),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        mgr,
    )
    .with_status_sink(sink.clone())
    .with_fusion(executor.clone());
    let mut input = workflow_input("await fusion('wait here');");
    if let TaskSpawnInput::LocalWorkflow {
        parent_model,
        parent_model_profile,
        ..
    } = &mut input
    {
        *parent_model = Some("gpt-5.4".into());
        *parent_model_profile = Some("openai".into());
    }
    let handle = handler.spawn(input, make_ctx(fs.clone())).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), started_rx)
        .await
        .expect("fusion start wait timed out")
        .expect("fusion started");
    handler.kill(&handle.task_id, make_ctx(fs)).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), cancelled_rx)
        .await
        .expect("fusion cancel wait timed out")
        .expect("fusion cancellation observed");
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), await_terminal(&sink))
            .await
            .expect("workflow kill status wait timed out"),
        TaskStatus::Killed
    );
    assert_eq!(executor.calls.load(AtomicOrdering::SeqCst), 1);
}

#[tokio::test]
async fn workflow_cleanup_cancels_an_inflight_fusion_before_runtime_abort() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let sink = Arc::new(RecordingSink::default());
    let (started_tx, started_rx) = oneshot::channel();
    let (cancelled_tx, cancelled_rx) = oneshot::channel();
    let executor = BlockingFusionExecutor::new(started_tx, cancelled_tx);
    let handler = LocalWorkflowHandler::new(
        Arc::new(EchoSpawner::default()),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        mgr,
    )
    .with_status_sink(sink.clone())
    .with_fusion(executor.clone());
    let mut input = workflow_input("await fusion('wait here');");
    if let TaskSpawnInput::LocalWorkflow {
        parent_model,
        parent_model_profile,
        ..
    } = &mut input
    {
        *parent_model = Some("gpt-5.4".into());
        *parent_model_profile = Some("openai".into());
    }
    let handle = handler.spawn(input, make_ctx(fs)).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), started_rx)
        .await
        .expect("fusion start wait timed out")
        .expect("fusion started");

    (handle.cleanup.as_ref().expect("cleanup seam"))();
    handler.drain_pending_kills().await;

    tokio::time::timeout(std::time::Duration::from_secs(5), cancelled_rx)
        .await
        .expect("fusion cancel wait timed out")
        .expect("fusion cancellation observed");
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(5), await_terminal(&sink))
            .await
            .expect("workflow cleanup status wait timed out"),
        TaskStatus::Killed
    );
    assert_eq!(executor.calls.load(AtomicOrdering::SeqCst), 1);
}

#[tokio::test]
async fn workflow_isolation_spawner_creates_worktree_and_threads_cwd() {
    let inner = Arc::new(EchoSpawner::default());
    let worktree = Arc::new(RecordingWorktreeManager::default());
    let spawner = WorkflowIsolationSpawner {
        process_owners: Default::default(),
        inner: inner.clone(),
        worktree: Some(worktree.clone()),
        slug_prefix: "w123".to_string(),
        sequence: AtomicU64::new(0),
        transcript_subdir: None,
    };
    let request = make_request(
        DEFAULT_WORKFLOW_SUBAGENT,
        "do isolated work",
        r#"{"isolation":"worktree"}"#,
    );

    let result = spawner
        .spawn(
            request,
            SubagentInheritance {
                tool_invoker: Arc::new(MockInvoker),
                budget: Arc::new(MockBudget),
            },
        )
        .await
        .expect("spawn succeeds");

    assert!(matches!(result, SubagentResult::Completed { .. }));
    let created = worktree.created();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].0, "workflow-agent-w123-0");
    let seen = inner.seen_reqs.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].isolation.as_deref(), Some("worktree"));
    let created_path = created[0].1.path.to_string_lossy().into_owned();
    assert_eq!(seen[0].cwd.as_deref(), Some(created_path.as_str()));
    assert_eq!(
        seen[0].worktree.as_ref().map(|h| h.branch_name.as_str()),
        Some(created[0].1.branch_name.as_str())
    );
}

#[tokio::test]
async fn workflow_isolation_spawner_forwards_live_observer_and_watchdog() {
    let inner = Arc::new(WorkflowForwardingProbeSpawner::default());
    let spawner = WorkflowIsolationSpawner {
        process_owners: Default::default(),
        inner: inner.clone(),
        worktree: None,
        slug_prefix: "workflow".to_string(),
        sequence: AtomicU64::new(0),
        transcript_subdir: None,
    };
    let observer = Arc::new(AllocationCountingObserver::default());
    let watchdog = platform_api::subagent_spawn::WorkflowQueryWatchdog {
        stall_timeout_ms: 1_234,
        max_retries: 2,
        retry_response_body: false,
    };
    let request = make_request(
        DEFAULT_WORKFLOW_SUBAGENT,
        "design the app",
        r#"{"model":"deepseek-flash","modelProfile":"deepseek"}"#,
    );

    spawner
        .spawn_workflow_with_observer(
            request,
            SubagentInheritance {
                tool_invoker: Arc::new(MockInvoker),
                budget: Arc::new(MockBudget),
            },
            None,
            Some(observer.clone()),
            watchdog,
        )
        .await
        .expect("workflow spawn succeeds");

    assert_eq!(
        inner.plain_spawns.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "wrapper must not degrade the workflow call to plain spawn"
    );
    assert_eq!(*inner.watchdogs.lock().unwrap(), vec![watchdog]);
    assert_eq!(*inner.observer_presence.lock().unwrap(), vec![true]);
    assert_eq!(
        observer
            .allocations
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[tokio::test]
async fn handler_runs_workflow_and_spools_the_return_value() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let spawner = Arc::new(EchoSpawner::default());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let sink = Arc::new(RecordingSink::default());

    let handler = make_handler(spawner.clone(), mgr.clone(), sink.clone());

    // A workflow that fans out two agents and returns a structured result.
    let script = r#"
        const rs = await parallel([() => agent('a'), () => agent('b')]);
        return { confirmed: rs };
    "#;
    let handle = handler
        .spawn(workflow_input(script), make_ctx(fs))
        .await
        .expect("spawn should succeed");

    assert!(
        handle.task_id.starts_with('w'),
        "LocalWorkflow id prefix 'w'"
    );
    assert!(handle.cleanup.is_some(), "cleanup seam present");

    assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);

    // Both agents ran.
    let mut seen = spawner.seen.lock().unwrap().clone();
    seen.sort();
    assert_eq!(seen, vec!["a".to_string(), "b".to_string()]);

    // The script's return value (JSON) was spooled as the task result,
    // after the surfaced run id.
    let spool_path = dir.path().join(format!("{}.output", handle.task_id));
    let read = mgr
        .read(&spool_path, crate::output_manager::OutputOptions::default())
        .await
        .unwrap();
    assert!(read.content.starts_with("runId: wf_"), "{}", read.content);
    assert!(
        read.content
            .contains(r#"{"confirmed":["echo:a","echo:b"]}"#),
        "{}",
        read.content
    );
}

#[tokio::test]
async fn handler_spools_live_progress_then_the_result() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let spawner = Arc::new(EchoSpawner::default());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let sink = Arc::new(RecordingSink::default());
    let handler = make_handler(spawner, mgr.clone(), sink.clone());

    let script = r#"
        phase('Scan');
        log('found 2 things');
        return { ok: true };
    "#;
    let handle = handler
        .spawn(workflow_input(script), make_ctx(fs))
        .await
        .unwrap();
    assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);

    let spool_path = dir.path().join(format!("{}.output", handle.task_id));
    let read = mgr
        .read(&spool_path, crate::output_manager::OutputOptions::default())
        .await
        .unwrap();
    // phase/log were spooled live, followed by the return value.
    // Phase now renders as "[N] === Title ===" (index-prefixed).
    assert!(
        read.content.contains("=== Scan ==="),
        "phase: {}",
        read.content
    );
    assert!(
        read.content.contains("[1]"),
        "phase index: {}",
        read.content
    );
    assert!(
        read.content.contains("found 2 things"),
        "log: {}",
        read.content
    );
    assert!(
        read.content.contains(r#"{"ok":true}"#),
        "result: {}",
        read.content
    );
}

#[tokio::test]
async fn handler_waits_for_registry_publication_before_reporting_status() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let spawner = Arc::new(EchoSpawner::default());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let sink = Arc::new(RegistrationSink::default());
    let handler = make_handler(spawner, mgr, sink.clone());

    let handle = handler
        .spawn(workflow_input("return 1;"), make_ctx(fs))
        .await
        .expect("spawn succeeds");

    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(
        sink.statuses().is_empty(),
        "worker must not report Running/Completed before the registry publishes the row"
    );

    sink.set_registered(true);
    // Wait on the condition, not on a fixed number of scheduler turns: the
    // worker's path between publication and its terminal status is not
    // bounded by any particular yield count, so counting them makes this
    // assert a guess about scheduling rather than about the handler.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !sink.last_status().is_some_and(TaskStatus::is_terminal) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker must reach a terminal status once the registry publishes");
    let statuses = sink.statuses();
    assert_eq!(
        statuses
            .iter()
            .map(|(_, status)| *status)
            .collect::<Vec<_>>(),
        vec![TaskStatus::Running, TaskStatus::Completed],
        "publication should unblock the normal Running→Completed sequence"
    );
    assert!(handle.task_id.starts_with('w'));
}

#[tokio::test]
async fn handler_registration_wait_exits_immediately_on_kill() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let spawner = Arc::new(EchoSpawner::default());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let sink = Arc::new(RegistrationSink::default());
    let handler = make_handler(spawner, mgr, sink);
    let ctx = make_ctx(fs.clone());

    let handle = handler
        .spawn(workflow_input("return 1;"), ctx.clone())
        .await
        .expect("spawn succeeds");

    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    handler
        .kill(&handle.task_id, ctx)
        .await
        .expect("kill succeeds during registration wait");

    for _ in 0..200 {
        if handler.workers.lock().await.is_empty() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("workflow worker did not exit registration wait after cancellation");
}

#[tokio::test]
async fn workflow_kill_preserves_terminal_status_when_sink_already_knows_task_is_done() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let sink = Arc::new(RecordingSink::default());
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let spawner = Arc::new(BlockingWorkflowObserverSpawner::new(started_tx));
    let handler = make_handler(spawner, mgr, sink.clone());
    let ctx = make_ctx(fs.clone());

    let handle = handler
        .spawn(workflow_input("await agent('blocked');"), ctx.clone())
        .await
        .expect("spawn succeeds");

    tokio::time::timeout(std::time::Duration::from_secs(2), started_rx.recv())
        .await
        .expect("workflow child should start")
        .expect("started prompt");

    sink.set_status(&handle.task_id, TaskStatus::Completed)
        .await;
    handler
        .kill(&handle.task_id, ctx)
        .await
        .expect("kill should succeed");

    assert_eq!(
        sink.last_status(),
        Some(TaskStatus::Completed),
        "kill must not overwrite an already-terminal workflow status"
    );
    assert!(
        handler.workers.lock().await.is_empty(),
        "a stale terminal status must not leave a live workflow worker orphaned"
    );
}

#[tokio::test]
async fn workflow_drain_pending_kills_preserves_terminal_status() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let sink = Arc::new(RecordingSink::default());
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let spawner = Arc::new(BlockingWorkflowObserverSpawner::new(started_tx));
    let handler = make_handler(spawner, mgr, sink.clone());

    let handle = handler
        .spawn(workflow_input("await agent('blocked');"), make_ctx(fs))
        .await
        .expect("spawn succeeds");

    tokio::time::timeout(std::time::Duration::from_secs(2), started_rx.recv())
        .await
        .expect("workflow child should start")
        .expect("started prompt");

    sink.set_status(&handle.task_id, TaskStatus::Completed)
        .await;
    (handle.cleanup.as_ref().expect("cleanup seam"))();
    handler.drain_pending_kills().await;

    assert_eq!(
        sink.last_status(),
        Some(TaskStatus::Completed),
        "drain must not overwrite an already-terminal workflow status"
    );
    assert!(
        handler.workers.lock().await.is_empty(),
        "cleanup must still tear down a live worker behind a stale terminal status"
    );
}

#[tokio::test]
async fn workflow_kill_preserves_terminalizing_outcome_window() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let sink = Arc::new(BlockingWorkflowTerminalSink::new(started_tx, release_rx));
    let handler = make_handler(Arc::new(EchoSpawner::default()), mgr, sink.clone());
    let ctx = make_ctx(fs);

    let handle = handler
        .spawn(workflow_input("return { ok: true };"), ctx.clone())
        .await
        .expect("spawn succeeds");

    tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
        .await
        .expect("workflow should enter terminal publish")
        .expect("terminal publish signal");

    handler
        .kill(&handle.task_id, ctx)
        .await
        .expect("kill should succeed while terminal publish is blocked");

    let _ = release_tx.send(());
    for _ in 0..400 {
        if sink.last_status().is_some_and(TaskStatus::is_terminal) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        sink.last_status(),
        Some(TaskStatus::Completed),
        "kill must not overwrite a workflow already committing its terminal outcome"
    );
    assert_eq!(sink.calls(), vec!["outcome", "status"]);
}

#[tokio::test]
async fn workflow_drain_preserves_terminalizing_outcome_window() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let sink = Arc::new(BlockingWorkflowTerminalSink::new(started_tx, release_rx));
    let handler = make_handler(Arc::new(EchoSpawner::default()), mgr, sink.clone());

    let handle = handler
        .spawn(workflow_input("return { ok: true };"), make_ctx(fs))
        .await
        .expect("spawn succeeds");

    tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
        .await
        .expect("workflow should enter terminal publish")
        .expect("terminal publish signal");

    (handle.cleanup.as_ref().expect("cleanup seam"))();
    handler.drain_pending_kills().await;

    let _ = release_tx.send(());
    for _ in 0..400 {
        if sink.last_status().is_some_and(TaskStatus::is_terminal) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        sink.last_status(),
        Some(TaskStatus::Completed),
        "drain must not overwrite a workflow already committing its terminal outcome"
    );
    assert_eq!(sink.calls(), vec!["outcome", "status"]);
}

#[tokio::test]
async fn workflow_cleanup_records_cancellation_even_when_worker_map_is_contended() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let sink = Arc::new(RecordingSink::default());
    let (started_tx, mut started_rx) = mpsc::unbounded_channel();
    let spawner = Arc::new(BlockingWorkflowObserverSpawner::new(started_tx));
    let handler = make_handler(spawner, mgr, sink.clone());
    let workers = handler.workers_map();

    let handle = handler
        .spawn(workflow_input("await agent('blocked');"), make_ctx(fs))
        .await
        .expect("spawn succeeds");

    tokio::time::timeout(std::time::Duration::from_secs(2), started_rx.recv())
        .await
        .expect("workflow child should start")
        .expect("started prompt");

    let workers_guard = workers.lock().await;
    (handle.cleanup.as_ref().expect("cleanup seam"))();
    drop(workers_guard);

    handler.drain_pending_kills().await;
    assert_eq!(
        await_terminal(&sink).await,
        TaskStatus::Killed,
        "cleanup must not silently lose cancellation when the worker map is contended"
    );
}

#[tokio::test]
async fn workflow_transcript_root_stays_pinned_across_retarget() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let spawner = Arc::new(TranscriptOverrideSpawner::default());
    let sink = Arc::new(RecordingSink::default());
    let handler = make_handler(spawner.clone(), mgr, sink.clone());
    let transcript_root = dir
        .path()
        .join("session-a")
        .join("subagents")
        .join("workflows")
        .join("wf_pin");

    let handle = handler
        .spawn(
            TaskSpawnInput::LocalWorkflow {
                session_uuid: None,
                workflow_id: "wf".into(),
                script: "await agent('a'); await agent('b'); return 'done';".into(),
                resume_from_run_id: None,
                args: None,
                run_id: Some("wf_pin".into()),
                parent_model: None,
                parent_model_profile: None,
                invocation_mode: Some("inline".to_string()),
                workflow_source: Some("inline".to_string()),
                script_is_verbatim_builtin: Some(false),
                transcript_subdir: Some(transcript_root.clone()),
                launched_from_subagent: false,
                tool_use_id: None,
                creator_teammate_name: None,
                creator_team_name: None,
                creator_agent_id: None,
                scope: None,
            },
            make_ctx(fs),
        )
        .await
        .expect("spawn succeeds");

    assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);
    let seen = spawner.overrides.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![Some(transcript_root.clone()), Some(transcript_root)]
    );
    assert!(handle.task_id.starts_with('w'));
}

#[tokio::test]
async fn workflow_transcript_dir_matches_child_transcript_location() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let spawner = Arc::new(TranscriptOverrideSpawner {
        transcript_lines: true,
        ..Default::default()
    });
    let sink = Arc::new(RecordingSink::default());
    let handler = make_handler(spawner, mgr, sink.clone());
    let transcript_root = dir
        .path()
        .join("session-a")
        .join("subagents")
        .join("workflows")
        .join("wf_real_dir");

    handler
        .spawn(
            TaskSpawnInput::LocalWorkflow {
                session_uuid: None,
                workflow_id: "wf".into(),
                script: "await agent('child'); return 'done';".into(),
                resume_from_run_id: None,
                args: None,
                run_id: Some("wf_real_dir".into()),
                parent_model: None,
                parent_model_profile: None,
                invocation_mode: Some("inline".to_string()),
                workflow_source: Some("inline".to_string()),
                script_is_verbatim_builtin: Some(false),
                transcript_subdir: Some(transcript_root.clone()),
                launched_from_subagent: false,
                tool_use_id: None,
                creator_teammate_name: None,
                creator_team_name: None,
                creator_agent_id: None,
                scope: None,
            },
            make_ctx(fs),
        )
        .await
        .expect("spawn succeeds");

    assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);
    let entries = std::fs::read_dir(&transcript_root)
        .expect("workflow transcript dir exists")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert!(
        entries.iter().any(|path| path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| { name.starts_with("agent-") && name.ends_with(".jsonl") })),
        "child transcript must be written inside the workflow transcript dir: {entries:?}"
    );
}

#[tokio::test]
async fn resume_replays_journaled_agent_results_without_respawning() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let script = r#"
        const a = await agent('a');
        const b = await agent('b');
        return { a, b };
    "#;

    // Run 1: fresh run — both agents spawn; capture the surfaced runId.
    let spawner1 = Arc::new(EchoSpawner::default());
    let sink1 = Arc::new(RecordingSink::default());
    let h1 = make_handler(spawner1.clone(), mgr.clone(), sink1.clone());
    let handle1 = h1
        .spawn(workflow_input(script), make_ctx(fs.clone()))
        .await
        .unwrap();
    assert_eq!(await_terminal(&sink1).await, TaskStatus::Completed);
    assert_eq!(spawner1.seen.lock().unwrap().len(), 2, "run 1 spawns both");

    let spool1 = dir.path().join(format!("{}.output", handle1.task_id));
    let out1 = mgr
        .read(&spool1, crate::output_manager::OutputOptions::default())
        .await
        .unwrap();
    let run_id = out1
        .content
        .lines()
        .find_map(|l| l.strip_prefix("runId: "))
        .expect("runId surfaced")
        .to_string();
    assert!(run_id.starts_with("wf_"), "runId: {run_id}");

    // Run 2: resume the same id — every agent() must replay from the journal,
    // so the spawner is never called, and the result is rebuilt from cache.
    let spawner2 = Arc::new(EchoSpawner::default());
    let sink2 = Arc::new(RecordingSink::default());
    let h2 = make_handler(spawner2.clone(), mgr.clone(), sink2.clone());
    let input2 = TaskSpawnInput::LocalWorkflow {
        session_uuid: None,
        workflow_id: "wf".into(),
        script: script.into(),
        resume_from_run_id: Some(run_id),
        args: None,
        run_id: None,
        parent_model: None,
        parent_model_profile: None,
        invocation_mode: Some("inline".to_string()),
        workflow_source: Some("inline".to_string()),
        script_is_verbatim_builtin: Some(false),
        transcript_subdir: None,
        launched_from_subagent: false,
        tool_use_id: None,
        creator_teammate_name: None,
        creator_team_name: None,
        creator_agent_id: None,
        scope: None,
    };
    let handle2 = h2.spawn(input2, make_ctx(fs.clone())).await.unwrap();
    assert_eq!(await_terminal(&sink2).await, TaskStatus::Completed);
    assert!(
        spawner2.seen.lock().unwrap().is_empty(),
        "resume must replay journaled results, not re-spawn"
    );

    let spool2 = dir.path().join(format!("{}.output", handle2.task_id));
    let out2 = mgr
        .read(&spool2, crate::output_manager::OutputOptions::default())
        .await
        .unwrap();
    assert!(
        out2.content.contains(r#"{"a":"echo:a","b":"echo:b"}"#),
        "rebuilt from cache: {}",
        out2.content
    );
}

#[tokio::test]
async fn transcript_journal_appends_started_and_result_before_resume() {
    let fs = Arc::new(InMemoryFs::new());
    let fs_trait: Arc<dyn FileSystem> = fs.clone();
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs_trait.clone(),
    ));
    let transcript_dir = dir
        .path()
        .join("session")
        .join("subagents")
        .join("workflows")
        .join("wf_append");
    let spawner = Arc::new(WorkflowForwardingProbeSpawner::default());
    let sink = Arc::new(RecordingSink::default());
    let handler = make_handler(spawner.clone(), mgr.clone(), sink.clone());
    let script = "return await agent('design');";

    handler
        .spawn(
            TaskSpawnInput::LocalWorkflow {
                session_uuid: None,
                workflow_id: "wf".into(),
                script: script.into(),
                resume_from_run_id: None,
                args: None,
                run_id: Some("wf_append".into()),
                parent_model: None,
                parent_model_profile: None,
                invocation_mode: Some("inline".into()),
                workflow_source: Some("inline".into()),
                script_is_verbatim_builtin: Some(false),
                transcript_subdir: Some(transcript_dir.clone()),
                launched_from_subagent: false,
                tool_use_id: None,
                creator_teammate_name: None,
                creator_team_name: None,
                creator_agent_id: None,
                scope: None,
            },
            make_ctx(fs_trait.clone()),
        )
        .await
        .unwrap();
    assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);

    let journal_path = transcript_dir.join("journal.jsonl");
    let journal = fs
        .read_file(journal_path.to_str().unwrap(), None, None)
        .await
        .unwrap()
        .content;
    let records = journal
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        records.len(),
        2,
        "started and result are append-only records"
    );
    assert_eq!(records[0]["type"], "started");
    assert_eq!(records[1]["type"], "result");
    assert_eq!(records[0]["key"], records[1]["key"]);
    assert_eq!(records[0]["agentId"], records[1]["agentId"]);

    let resumed_spawner = Arc::new(WorkflowForwardingProbeSpawner::default());
    let resumed_sink = Arc::new(RecordingSink::default());
    let resumed = make_handler(resumed_spawner.clone(), mgr, resumed_sink.clone());
    resumed
        .spawn(
            TaskSpawnInput::LocalWorkflow {
                session_uuid: None,
                workflow_id: "wf".into(),
                script: script.into(),
                resume_from_run_id: Some("wf_append".into()),
                args: None,
                run_id: None,
                parent_model: None,
                parent_model_profile: None,
                invocation_mode: Some("inline".into()),
                workflow_source: Some("inline".into()),
                script_is_verbatim_builtin: Some(false),
                transcript_subdir: Some(transcript_dir),
                launched_from_subagent: false,
                tool_use_id: None,
                creator_teammate_name: None,
                creator_team_name: None,
                creator_agent_id: None,
                scope: None,
            },
            make_ctx(fs_trait),
        )
        .await
        .unwrap();
    assert_eq!(await_terminal(&resumed_sink).await, TaskStatus::Completed);
    assert!(
        resumed_spawner.watchdogs.lock().unwrap().is_empty(),
        "a resumed cached prefix must not spawn the child again"
    );
}

#[tokio::test]
async fn label_opt_becomes_the_subagent_display_name() {
    let spawner = Arc::new(EchoSpawner::default());
    run_bridge(
        "await agent('p', { label: 'my-label' }); await agent('q');",
        spawner.clone(),
    )
    .await;
    let reqs = spawner.seen_reqs.lock().unwrap().clone();
    assert_eq!(reqs[0].name.as_deref(), Some("my-label"), "label → name");
    assert_eq!(reqs[1].name, None, "no label → no name");
}

#[tokio::test]
async fn resume_with_a_changed_prefix_reruns_from_the_edit_onward() {
    // PREFIX semantics: editing the FIRST agent's prompt on resume must
    // re-run it AND every later call (the chained key cascades) — NOT replay
    // the now-misaligned journaled results by a flat (prompt,opts) match.
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));

    // Run 1: journal agents 'a' then 'b'.
    let script1 = "const a = await agent('a'); const b = await agent('b'); return { a, b };";
    let spawner1 = Arc::new(EchoSpawner::default());
    let sink1 = Arc::new(RecordingSink::default());
    let h1 = make_handler(spawner1.clone(), mgr.clone(), sink1.clone());
    let handle1 = h1
        .spawn(workflow_input(script1), make_ctx(fs.clone()))
        .await
        .unwrap();
    assert_eq!(await_terminal(&sink1).await, TaskStatus::Completed);
    let spool1 = dir.path().join(format!("{}.output", handle1.task_id));
    let out1 = mgr
        .read(&spool1, crate::output_manager::OutputOptions::default())
        .await
        .unwrap();
    let run_id = out1
        .content
        .lines()
        .find_map(|l| l.strip_prefix("runId: "))
        .expect("runId")
        .to_string();

    // Run 2: resume the same id but EDIT the first prompt ('a' → 'a2'). Both
    // agents must re-spawn — 'b' too, because its key chains off the changed
    // 'a2'. The flat-map cache would have wrongly replayed 'b' from run 1.
    let script2 = "const a = await agent('a2'); const b = await agent('b'); return { a, b };";
    let spawner2 = Arc::new(EchoSpawner::default());
    let sink2 = Arc::new(RecordingSink::default());
    let h2 = make_handler(spawner2.clone(), mgr.clone(), sink2.clone());
    let input2 = TaskSpawnInput::LocalWorkflow {
        session_uuid: None,
        workflow_id: "wf".into(),
        script: script2.into(),
        resume_from_run_id: Some(run_id),
        args: None,
        run_id: None,
        parent_model: None,
        parent_model_profile: None,
        invocation_mode: Some("inline".to_string()),
        workflow_source: Some("inline".to_string()),
        script_is_verbatim_builtin: Some(false),
        transcript_subdir: None,
        launched_from_subagent: false,
        tool_use_id: None,
        creator_teammate_name: None,
        creator_team_name: None,
        creator_agent_id: None,
        scope: None,
    };
    h2.spawn(input2, make_ctx(fs.clone())).await.unwrap();
    assert_eq!(await_terminal(&sink2).await, TaskStatus::Completed);
    let mut seen = spawner2.seen.lock().unwrap().clone();
    seen.sort();
    assert_eq!(
        seen,
        vec!["a2".to_string(), "b".to_string()],
        "edited prefix re-runs the edit AND everything after it"
    );
}

#[tokio::test]
async fn budget_total_and_own_spend_drive_the_budget_global() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let spawner = Arc::new(EchoSpawner::default());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let sink = Arc::new(RecordingSink::default());
    let handler = make_handler(spawner, mgr.clone(), sink.clone()).with_token_budget(Some(500));

    // Each echo agent reports 100 output tokens; two agents ⇒ spent 200.
    let script = r#"
        await agent('a');
        await agent('b');
        log('B:' + budget.total + '/' + budget.spent() + '/' + budget.remaining());
        return {};
    "#;
    let handle = handler
        .spawn(workflow_input(script), make_ctx(fs))
        .await
        .unwrap();
    assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);

    let spool = dir.path().join(format!("{}.output", handle.task_id));
    let read = mgr
        .read(&spool, crate::output_manager::OutputOptions::default())
        .await
        .unwrap();
    // total=500, spent=2×100, remaining=300.
    assert!(read.content.contains("B:500/200/300"), "{}", read.content);
}

/// Finding [12]: a `fusion()` call that ends in `Err` after the orchestrator
/// already priced and billed real panel spend must still advance the
/// workflow's own token budget (`spent`) by that amount — otherwise the
/// `:2535`-area gate and a script's own `budget.remaining()` under-report,
/// letting a `while (budget.remaining() > N) { try fusion() catch {} }` loop
/// run far more real, billed panel fan-outs than the budget was meant to
/// allow.
#[tokio::test]
async fn a_failed_fusion_call_still_charges_its_realized_output_tokens_to_the_workflow_budget() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let spawner = Arc::new(EchoSpawner::default());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let sink = Arc::new(RecordingSink::default());
    let executor = Arc::new(FailingAfterRealSpendFusionExecutor {
        realized_output_tokens: 250,
    });
    let handler = LocalWorkflowHandler::new(
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        mgr.clone(),
    )
    .with_status_sink(sink.clone())
    .with_fusion(executor)
    .with_token_budget(Some(1_000));

    let script = r#"
        let caught = false;
        try {
            await fusion('review this');
        } catch (e) {
            caught = true;
        }
        log('caught:' + caught + ' spent:' + budget.spent() + ' remaining:' + budget.remaining());
        return {};
    "#;
    let mut input = workflow_input(script);
    if let TaskSpawnInput::LocalWorkflow {
        parent_model,
        parent_model_profile,
        ..
    } = &mut input
    {
        *parent_model = Some("gpt-5.4".into());
        *parent_model_profile = Some("openai".into());
    }
    let handle = handler.spawn(input, make_ctx(fs.clone())).await.unwrap();
    assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);

    let spool = dir.path().join(format!("{}.output", handle.task_id));
    let read = mgr
        .read(&spool, crate::output_manager::OutputOptions::default())
        .await
        .unwrap();
    assert!(
        read.content.contains("caught:true"),
        "the fusion() call must have thrown: {}",
        read.content
    );
    assert!(
        read.content.contains("spent:250"),
        "the failed fusion() call's realized_output_tokens (250) must reach \
budget.spent() even though the call ended in Err: {}",
        read.content
    );
    assert!(
        read.content.contains("remaining:750"),
        "budget.remaining() (1000 total) must drop by the realized spend even on \
a failed fusion() call: {}",
        read.content
    );
}

#[tokio::test]
async fn handler_maps_a_script_error_to_failed() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let spawner = Arc::new(EchoSpawner::default());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let sink = Arc::new(RecordingSink::default());

    let handler = make_handler(spawner, mgr, sink.clone());

    // A script that throws ⇒ WorkflowError::Script ⇒ Failed.
    let handle = handler
        .spawn(workflow_input("throw new Error('kaboom');"), make_ctx(fs))
        .await
        .unwrap();

    assert_eq!(await_terminal(&sink).await, TaskStatus::Failed);
    assert!(handle.task_id.starts_with('w'));
}

#[tokio::test]
async fn handler_rejects_a_non_workflow_input() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let handler = make_handler(
        Arc::new(EchoSpawner::default()),
        mgr,
        Arc::new(RecordingSink::default()),
    );

    let wrong = TaskSpawnInput::LocalAgent {
        agent_id: protocol::AgentId::new(),
        subagent_type: "general-purpose".into(),
        prompt: "p".into(),
        is_backgrounded: true,
        tool_use_id: None,
        creator_teammate_name: None,
        creator_team_name: None,
        creator_agent_id: None,
        spawn_request: None,
        inheritance: None,
    };
    match handler.spawn(wrong, make_ctx(fs)).await {
        Err(TaskError::Internal(_)) => {}
        Err(other) => panic!("expected Internal, got {other:?}"),
        Ok(_) => panic!("non-LocalWorkflow input must be rejected"),
    }
}

#[tokio::test]
async fn name_and_type_are_local_workflow() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(PathBuf::from(dir.path()), fs));
    let handler = make_handler(
        Arc::new(EchoSpawner::default()),
        mgr,
        Arc::new(RecordingSink::default()),
    );
    assert_eq!(handler.name(), "local_workflow");
    assert_eq!(handler.task_type(), TaskType::LocalWorkflow);
}

// ==== agent() routing tests (Cases 1-4 + §6) ============================

/// Case 1: bare `agent(prompt)` → subagent_type = "workflow-subagent",
/// no system_prompt_override, no addendum, no additional disallowed tools
/// (the builtin def already has {SendUserMessage, Agent, Workflow}).
#[test]
fn bare_agent_routes_to_workflow_subagent_with_kbp() {
    let req = make_request(DEFAULT_WORKFLOW_SUBAGENT, "do something", "{}");
    assert_eq!(req.subagent_type, "workflow-subagent");
    assert!(
        req.system_prompt_override.is_none(),
        "no prompt override for bare agent()"
    );
    assert!(
        req.system_prompt_addendum.is_none(),
        "no addendum for bare agent()"
    );
    assert!(
        req.additional_disallowed_tools.is_empty(),
        "no extra disallowed for bare agent()"
    );
}

/// Case 2: `agent(prompt, {schema})` (no agentType) → workflow-subagent +
/// system_prompt_override = xBp.
#[test]
fn bare_schema_agent_uses_xbp() {
    let req = make_request(
        DEFAULT_WORKFLOW_SUBAGENT,
        "return structured",
        r#"{"schema":{"type":"object","properties":{"count":{"type":"number"}}}}"#,
    );
    assert_eq!(req.subagent_type, "workflow-subagent");
    // Override must be the xBp string (WORKFLOW_SUBAGENT_SCHEMA_PROMPT).
    let override_prompt = req
        .system_prompt_override
        .as_deref()
        .expect("override must be set for schema agent()");
    assert_eq!(
        override_prompt,
        agent::builtins::WORKFLOW_SUBAGENT_SCHEMA_PROMPT
    );
    assert!(
        req.system_prompt_addendum.is_none(),
        "no addendum when no explicit agentType"
    );
    assert!(
        req.additional_disallowed_tools.is_empty(),
        "no extra disallowed for bare schema agent()"
    );
}

/// Case 3: `agent(prompt, {agentType})` (no schema) → that agentType, HBp
/// addendum appended, disallow union {SendUserMessage, Agent, Workflow}.
#[test]
fn user_agenttype_gets_hbp_addendum_and_disallow_union() {
    let req = make_request(
        DEFAULT_WORKFLOW_SUBAGENT,
        "analyze code",
        r#"{"agentType":"general-purpose"}"#,
    );
    assert_eq!(req.subagent_type, "general-purpose");
    assert!(
        req.system_prompt_override.is_none(),
        "no prompt override for user agentType"
    );
    let addendum = req
        .system_prompt_addendum
        .as_deref()
        .expect("HBp addendum must be set");
    assert_eq!(
        addendum,
        agent::builtins::WORKFLOW_SUBAGENT_NON_SCHEMA_ADDENDUM
    );
    // Must request union with {SendUserMessage, Agent, Workflow}.
    let disallowed = &req.additional_disallowed_tools;
    assert!(
        disallowed.contains(&"SendUserMessage".to_string()),
        "SendUserMessage must be disallowed: {disallowed:?}"
    );
    assert!(
        disallowed.contains(&"Agent".to_string()),
        "Agent must be disallowed: {disallowed:?}"
    );
    assert!(
        disallowed.contains(&"Workflow".to_string()),
        "Workflow must be disallowed: {disallowed:?}"
    );
}

/// Case 4: `agent(prompt, {agentType, schema})` → that agentType, IBp
/// addendum appended, disallow union set.
#[test]
fn user_agenttype_with_schema_gets_ibp_and_disallow_union() {
    let req = make_request(
        DEFAULT_WORKFLOW_SUBAGENT,
        "return structured",
        r#"{"agentType":"code-reviewer","schema":{"type":"object"}}"#,
    );
    assert_eq!(req.subagent_type, "code-reviewer");
    assert!(
        req.system_prompt_override.is_none(),
        "no prompt override for user agentType"
    );
    let addendum = req
        .system_prompt_addendum
        .as_deref()
        .expect("IBp addendum must be set");
    assert_eq!(addendum, agent::builtins::WORKFLOW_SUBAGENT_SCHEMA_ADDENDUM);
    // Must request union with {SendUserMessage, Agent, Workflow}.
    let disallowed = &req.additional_disallowed_tools;
    assert!(
        disallowed.contains(&"SendUserMessage".to_string()),
        "SendUserMessage must be disallowed: {disallowed:?}"
    );
    assert!(
        disallowed.contains(&"Agent".to_string()),
        "Agent must be disallowed: {disallowed:?}"
    );
    assert!(
        disallowed.contains(&"Workflow".to_string()),
        "Workflow must be disallowed: {disallowed:?}"
    );
}

// ---- chain_key / normalize_opts_for_chain_key ---------------------------

/// Display-only opts (`phase`, `label`, `stallMs`) must NOT change the chain
/// key — they are stripped by `normalize_opts_for_chain_key` (ABp parity).
#[test]
fn chain_key_ignores_display_only_opts() {
    let opts_a = r#"{"model":"claude-opus-4","phase":"research","label":"step1"}"#;
    let opts_b = r#"{"model":"claude-opus-4","phase":"writing","label":"step2","stallMs":5000}"#;
    let key_a = chain_key(
        "",
        "do something",
        &normalize_opts_for_chain_key(&serde_json::from_str(opts_a).unwrap()),
    );
    let key_b = chain_key(
        "",
        "do something",
        &normalize_opts_for_chain_key(&serde_json::from_str(opts_b).unwrap()),
    );
    assert_eq!(
        key_a, key_b,
        "display-only fields must not affect the chain key"
    );
}

/// Changing `model` (an identity key) MUST produce a different chain key.
#[test]
fn chain_key_differs_on_model_change() {
    let opts_a = r#"{"model":"claude-opus-4"}"#;
    let opts_b = r#"{"model":"claude-sonnet-4"}"#;
    let key_a = chain_key(
        "",
        "do something",
        &normalize_opts_for_chain_key(&serde_json::from_str(opts_a).unwrap()),
    );
    let key_b = chain_key(
        "",
        "do something",
        &normalize_opts_for_chain_key(&serde_json::from_str(opts_b).unwrap()),
    );
    assert_ne!(
        key_a, key_b,
        "different model must produce different chain key"
    );
}

/// Provider identity is part of the extended workflow model reference. Two
/// providers can expose the same wire model id and must not share cached output.
#[test]
fn chain_key_differs_on_model_profile_change() {
    let opts_a = r#"{"model":"gpt-5.5","modelProfile":"openai"}"#;
    let opts_b = r#"{"model":"gpt-5.5","modelProfile":"github-copilot"}"#;
    let key_a = chain_key(
        "",
        "do something",
        &normalize_opts_for_chain_key(&serde_json::from_str(opts_a).unwrap()),
    );
    let key_b = chain_key(
        "",
        "do something",
        &normalize_opts_for_chain_key(&serde_json::from_str(opts_b).unwrap()),
    );
    assert_ne!(
        key_a, key_b,
        "different providers must not share a cache key"
    );
}

#[test]
fn chain_key_canonicalizes_model_profile_alias() {
    let camel = r#"{"model":"deepseek-flash","modelProfile":"deepseek"}"#;
    let snake = r#"{"model":"deepseek-flash","model_profile":"deepseek"}"#;
    let camel_key = chain_key(
        "",
        "do something",
        &normalize_opts_for_chain_key(&serde_json::from_str(camel).unwrap()),
    );
    let snake_key = chain_key(
        "",
        "do something",
        &normalize_opts_for_chain_key(&serde_json::from_str(snake).unwrap()),
    );
    assert_eq!(camel_key, snake_key);
}

/// Key order in the raw opts JSON must NOT matter — normalization sorts keys.
#[test]
fn chain_key_stable_regardless_of_input_key_order() {
    let opts_a = r#"{"model":"claude-opus-4","schema":{"type":"object"}}"#;
    let opts_b = r#"{"schema":{"type":"object"},"model":"claude-opus-4"}"#;
    let key_a = chain_key(
        "",
        "do something",
        &normalize_opts_for_chain_key(&serde_json::from_str(opts_a).unwrap()),
    );
    let key_b = chain_key(
        "",
        "do something",
        &normalize_opts_for_chain_key(&serde_json::from_str(opts_b).unwrap()),
    );
    assert_eq!(
        key_a, key_b,
        "key order in opts JSON must not affect the chain key"
    );
}

/// [finding 21] `fusion("X")`'s chain key must be disjoint from
/// `agent("fusion:X")`'s by CONSTRUCTION — not by a `fusion:` text prefix
/// folded into the same hashed `prompt` slot agent() also hashes over, which
/// an agent() prompt that literally starts with `fusion:` can also spell.
/// The two entry points must key-collide with NEITHER direction: a resumed
/// run that was edited from one call shape to the other must not silently
/// replay the wrong kind of cached result (a serialized `FusionResult` JSON
/// handed to the script as a subagent's "final text", or the reverse: a
/// `fusion() host returned invalid JSON` error because a plain agent reply
/// was fed back through the fusion decode path).
#[test]
fn fusion_chain_key_never_collides_with_an_agent_prompt_spelled_fusion_colon() {
    let opts = normalize_fusion_opts_for_chain_key("{}");
    // What `fusion("pick the best plan")` computes.
    let fusion_key = fusion_chain_key("", "pick the best plan", &opts);
    // What `agent("fusion:pick the best plan")` computes for the SAME
    // no-opts case (`normalize_opts_for_chain_key(&json!({}))` also
    // serializes to `"{}"`, so the opts argument agrees too).
    let agent_key = chain_key("", "fusion:pick the best plan", &opts);
    assert_ne!(
        fusion_key, agent_key,
        "fusion(\"X\") must not hash to the same key as agent(\"fusion:X\")"
    );
}

/// The fusion-side discriminator fold must not perturb `agent()`'s own
/// `chain_key` bytes — an existing on-disk journal (written by a prior
/// release, before this fix) must keep resuming correctly, not turn into a
/// wholesale cache miss the moment this ships. Pinned against an
/// independent, hand-rolled FNV-1a-64 fold of `prev | 0x1e | prompt | 0x1f |
/// opts` (the documented algorithm) rather than a call back into
/// `chain_key` itself, so a future accidental change to the fold order
/// cannot silently drag both sides along together.
#[test]
fn agent_chain_key_bytes_are_unchanged_by_the_fusion_discriminator_fix() {
    fn reference_fnv1a(prev: &str, prompt: &str, opts_json: &str) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut fold = |bytes: &[u8]| {
            for &b in bytes {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        fold(prev.as_bytes());
        fold(b"\x1e");
        fold(prompt.as_bytes());
        fold(b"\x1f");
        fold(opts_json.as_bytes());
        format!("{h:016x}")
    }
    let opts = normalize_opts_for_chain_key(&serde_json::from_str("{}").unwrap());
    assert_eq!(
        chain_key("prev", "do something", &opts),
        reference_fnv1a("prev", "do something", &opts),
    );
}

/// [Finding 19] `chain_key_with_discriminator`'s doc comment claims the
/// `\x1d`-wrapped discriminator makes the two key spaces disjoint "BY
/// CONSTRUCTION" because `\x1d` "never appears in `prev`/`prompt`/
/// `opts_json` text" — but `\x1d` (U+001D, GROUP SEPARATOR) is a valid
/// single-byte UTF-8 character, so an `agent()` prompt that happens to
/// SPELL the exact framing bytes (`\x1d` + tag + `\x1d`) reproduces a
/// `fusion()` key byte-for-byte at the same chain position with matching
/// opts. Independently verified via a standalone FNV-1a-64 fold: both
/// `fold("" | \x1e | \x1dfusion\x1d | "pick the best plan" | \x1f | "{}")`
/// and `fold("" | \x1e | \x1d | "fusion" | \x1d | "pick the best plan" |
/// \x1f | "{}")` hash to `5fdcc3407edae3a1`. This must go RED on the
/// original `\x1d`-wrapped fold and GREEN once the discriminator framing
/// uses a byte that can never occur in a valid Rust `&str` (guaranteed
/// UTF-8), such as `0xFF`.
#[test]
fn fusion_chain_key_never_collides_with_an_agent_prompt_spelling_the_discriminator_framing() {
    let opts = normalize_fusion_opts_for_chain_key("{}");
    let fusion_key = fusion_chain_key("", "pick the best plan", &opts);
    // An agent() prompt that spells the discriminator's OWN framing bytes
    // around the tag text, rather than a human-typed "fusion:" prefix.
    let crafted_agent_prompt = "\u{1d}fusion\u{1d}pick the best plan";
    let agent_key = chain_key("", crafted_agent_prompt, &opts);
    assert_ne!(
        fusion_key, agent_key,
        "an agent() prompt spelling the discriminator's own \\x1d-framing \
         bytes must not reproduce a fusion() key -- got {fusion_key} for both"
    );
}

/// Verify the concurrency cap formula: Math.min(16, Math.max(2, cpus-2)).
/// At 1–3 cores the floor is 2; at 5 cores it's 3; at 18 cores it's capped at 16.
#[test]
fn concurrency_cap_formula_matches_binary() {
    // Calls the PRODUCTION function. This previously re-declared the formula as
    // a local closure and asserted against that, so it stayed green no matter
    // what `concurrency_cap_default` did.
    assert_eq!(concurrency_cap_default(1), 2, "1 core → 2");
    assert_eq!(concurrency_cap_default(2), 2, "2 cores → 2");
    assert_eq!(concurrency_cap_default(3), 2, "3 cores → 2");
    assert_eq!(concurrency_cap_default(4), 2, "4 cores → 2");
    assert_eq!(concurrency_cap_default(5), 3, "5 cores → 3");
    assert_eq!(concurrency_cap_default(10), 8, "10 cores → 8");
    assert_eq!(concurrency_cap_default(18), 16, "18 cores → 16 (cap)");
    assert_eq!(concurrency_cap_default(128), 16, "still 16 (cap)");
}

#[test]
fn workflow_script_size_uses_javascript_utf16_length() {
    assert_eq!(workflow_script_size_chars("abc"), 3);
    assert_eq!(workflow_script_size_chars("😀"), 2);
    assert_eq!(workflow_script_size_chars("a😀b"), 4);
}

#[test]
fn telemetry_names_are_redacted_unless_the_script_is_a_verbatim_builtin() {
    assert_eq!(
        telemetry_workflow_name(Some("built-in"), Some(true), Some("review-changes")),
        "review-changes"
    );
    assert_eq!(
        telemetry_workflow_name(Some("built-in"), Some(true), None),
        "custom"
    );
    assert_eq!(
        telemetry_workflow_name(Some("inline"), Some(false), Some("review-changes")),
        "custom"
    );
    assert_eq!(
        telemetry_workflow_name(Some("scriptPath"), Some(false), Some("review-changes")),
        "custom"
    );
    assert_eq!(
        telemetry_workflow_name(Some("built-in"), Some(false), Some("review-changes")),
        "custom"
    );
    assert_eq!(
        telemetry_workflow_description(Some("built-in"), Some(true), Some("keep me")),
        "keep me"
    );
    assert_eq!(
        telemetry_workflow_description(Some("inline"), Some(false), Some("secret")),
        ""
    );
    assert_eq!(
        telemetry_workflow_description(Some("built-in"), Some(false), Some("secret")),
        ""
    );
    let long = "😀".repeat(120);
    let sliced = telemetry_workflow_description(Some("built-in"), Some(true), Some(&long));
    assert_eq!(sliced.encode_utf16().count(), 200);
}

// ==== Telemetry tests ====================================================

/// `tengu_workflow_phase_completed` does NOT fire when the script has no
/// `phase()` calls (bridge-level — verifies the post-run emit loop is a no-op
/// when `outcome.progress` has no Phase entries).
#[tokio::test]
async fn telemetry_no_phase_events_without_phase_calls() {
    use telemetry::InMemorySink;
    let sink = Arc::new(InMemorySink::default());
    let bus = Arc::new(AnalyticsBus::new());
    bus.attach_sink(sink.clone()).await;

    let spawner = Arc::new(EchoSpawner::default());
    run_workflow_script(
        "return 'ok';",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        bus.clone(),
        None,
        None,
    )
    .await
    .expect("runs");

    let events = sink.events().await;
    assert!(
        !events
            .iter()
            .any(|e| e.name == telemetry::tengu::workflow::PHASE_COMPLETED),
        "no phase_completed for a script with no phase() calls; events: {events:?}"
    );
}

/// `tengu_workflow_phase_completed` fires once per `phase()` call for a NAMED
/// (built-in source) workflow — oracle §7 gating condition.
#[tokio::test]
async fn telemetry_phase_completed_fires_per_phase_for_named_workflow() {
    use telemetry::InMemorySink;
    let sink = Arc::new(InMemorySink::default());
    let bus = Arc::new(AnalyticsBus::new());
    bus.attach_sink(sink.clone()).await;

    let spawner = Arc::new(EchoSpawner::default());
    run_workflow_script(
        "phase('Step 1'); await agent('phase-1'); phase('Step 2'); await agent('phase-2'); return 'done';",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        bus.clone(),
        None,
        // Pass a named invocation_mode — oracle §7: only "named" (built-in source)
        // emits tengu_workflow_phase_completed.
        Some(PhaseTelemetryCtx {
            run_id: "wf_test".to_string(),
            workflow_source: Some("built-in".to_string()),
            script_is_verbatim_builtin: Some(true),
            workflow_name: Some("My Workflow".to_string()),
            invocation_mode: Some("named".to_string()),
        }),
    )
    .await
    .expect("runs");

    let events: Vec<_> = sink.events().await;
    let phase_events: Vec<_> = events
        .iter()
        .filter(|e| e.name == telemetry::tengu::workflow::PHASE_COMPLETED)
        .collect();
    assert_eq!(
        phase_events.len(),
        2,
        "one event per phase() for named workflow; got {phase_events:?}"
    );
    assert!(
        matches!(phase_events[0].metadata.get("phase_title"), Some(AnalyticsValue::String(s)) if s == "Step 1"),
        "first phase title"
    );
    // Current Claude Code keeps phaseIndex 1-based for both workflow progress
    // and `tengu_workflow_phase_completed` telemetry.
    assert!(
        matches!(
            phase_events[0].metadata.get("phase_index"),
            Some(AnalyticsValue::Int(1))
        ),
        "first phase index (1-based in current Claude Code telemetry)"
    );
    assert!(
        matches!(phase_events[1].metadata.get("phase_title"), Some(AnalyticsValue::String(s)) if s == "Step 2"),
        "second phase title"
    );
    assert!(
        matches!(
            phase_events[1].metadata.get("phase_index"),
            Some(AnalyticsValue::Int(2))
        ),
        "second phase index (1-based in current Claude Code telemetry)"
    );
}

#[tokio::test]
async fn telemetry_phase_completed_includes_phase_only_workflows() {
    use telemetry::InMemorySink;
    let sink = Arc::new(InMemorySink::default());
    let bus = Arc::new(AnalyticsBus::new());
    bus.attach_sink(sink.clone()).await;

    run_workflow_script(
        "phase('Empty 1'); log('no agents'); phase('Empty 2'); return 'done';",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        Arc::new(EchoSpawner::default()),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        bus.clone(),
        None,
        Some(PhaseTelemetryCtx {
            run_id: "wf_test_phase_only".to_string(),
            workflow_source: Some("built-in".to_string()),
            script_is_verbatim_builtin: Some(true),
            workflow_name: Some("phase-only".to_string()),
            invocation_mode: Some("named".to_string()),
        }),
    )
    .await
    .expect("runs");

    let events = sink.events().await;
    let phase_events: Vec<_> = events
        .iter()
        .filter(|event| event.name == telemetry::tengu::workflow::PHASE_COMPLETED)
        .collect();
    assert_eq!(phase_events.len(), 2);
    assert!(matches!(
        phase_events[0].metadata.get("phase_title"),
        Some(AnalyticsValue::String(title)) if title == "Empty 1"
    ));
    assert!(matches!(
        phase_events[0].metadata.get("phase_agent_count"),
        Some(AnalyticsValue::Int(0))
    ));
    assert!(matches!(
        phase_events[1].metadata.get("phase_title"),
        Some(AnalyticsValue::String(title)) if title == "Empty 2"
    ));
}

/// `tengu_workflow_phase_completed` does NOT fire for an INLINE script, even if
/// it calls `phase()` — oracle §7 gates this event on `p.source === "built-in"`
/// (invocation_mode == "named") only.
#[tokio::test]
async fn telemetry_phase_completed_suppressed_for_inline_workflow() {
    use telemetry::InMemorySink;
    let sink = Arc::new(InMemorySink::default());
    let bus = Arc::new(AnalyticsBus::new());
    bus.attach_sink(sink.clone()).await;

    let spawner = Arc::new(EchoSpawner::default());
    run_workflow_script(
        "phase('Step 1'); phase('Step 2'); return 'done';",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        bus.clone(),
        None,
        // Inline invocation_mode: oracle §7 suppresses phase_completed for
        // "inline" (and "scriptPath") — only "named" (built-in source) emits it.
        Some(PhaseTelemetryCtx {
            run_id: "wf_test_inline".to_string(),
            workflow_source: Some("inline".to_string()),
            script_is_verbatim_builtin: Some(false),
            workflow_name: None,
            invocation_mode: Some("inline".to_string()),
        }),
    )
    .await
    .expect("runs");

    let events = sink.events().await;
    assert!(
        !events
            .iter()
            .any(|e| e.name == telemetry::tengu::workflow::PHASE_COMPLETED),
        "tengu_workflow_phase_completed must NOT fire for inline workflows (oracle §7); events: {events:?}"
    );
}

/// `tengu_workflow_phase_completed` does NOT fire for a `scriptPath` workflow,
/// even if it calls `phase()` — oracle §7 gates this on `p.source === "built-in"`
/// (invocation_mode == "named") only. scriptPath is not a saved/built-in source.
#[tokio::test]
async fn telemetry_phase_completed_suppressed_for_script_path_workflow() {
    use telemetry::InMemorySink;
    let sink = Arc::new(InMemorySink::default());
    let bus = Arc::new(AnalyticsBus::new());
    bus.attach_sink(sink.clone()).await;

    let spawner = Arc::new(EchoSpawner::default());
    run_workflow_script(
        "phase('Step A'); return 'done';",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        bus.clone(),
        None,
        Some(PhaseTelemetryCtx {
            run_id: "wf_test_scriptpath".to_string(),
            workflow_source: Some("/path/to/workflow.js".to_string()),
            script_is_verbatim_builtin: Some(false),
            workflow_name: None,
            invocation_mode: Some("scriptPath".to_string()),
        }),
    )
    .await
    .expect("runs");

    let events = sink.events().await;
    assert!(
        !events
            .iter()
            .any(|e| e.name == telemetry::tengu::workflow::PHASE_COMPLETED),
        "tengu_workflow_phase_completed must NOT fire for scriptPath workflows (oracle §7); events: {events:?}"
    );
}

/// `tengu_workflow_phase_completed` does NOT fire when phase_telemetry_ctx is None
/// (the bridge-level path without full context — verifies gating predicate).
#[tokio::test]
async fn telemetry_phase_completed_suppressed_when_no_telemetry_ctx() {
    use telemetry::InMemorySink;
    let sink = Arc::new(InMemorySink::default());
    let bus = Arc::new(AnalyticsBus::new());
    bus.attach_sink(sink.clone()).await;

    let spawner = Arc::new(EchoSpawner::default());
    run_workflow_script(
        "phase('Step 1'); return 'done';",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        bus.clone(),
        None,
        // No PhaseTelemetryCtx → is_named_source = false → no phase_completed events.
        // Note: run_bridge() / test helpers typically pass None here; this test
        // documents that the gate also protects the None case (no ctx = not named).
        None,
    )
    .await
    .expect("runs");

    let events = sink.events().await;
    assert!(
        !events
            .iter()
            .any(|e| e.name == telemetry::tengu::workflow::PHASE_COMPLETED),
        "tengu_workflow_phase_completed must NOT fire when phase_telemetry_ctx is None; events: {events:?}"
    );
}

/// `tengu_workflow_budget_cap_exceeded` fires when the budget ceiling is hit.
#[tokio::test]
async fn telemetry_budget_cap_fires() {
    use std::sync::atomic::AtomicU64;
    use telemetry::InMemorySink;
    let sink = Arc::new(InMemorySink::default());
    let bus = Arc::new(AnalyticsBus::new());
    bus.attach_sink(sink.clone()).await;

    // total=100; pool already at 150 this turn (baseline 0) → 150 >= 100.
    let pool = Arc::new(AtomicU64::new(150));
    let spawner = Arc::new(EchoSpawner::default());
    let _ = run_workflow_script(
        "await agent('a'); return 'done';",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        Some(100),
        Some(pool),
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        bus.clone(),
        None,
        None,
    )
    .await; // expected Err

    let events: Vec<_> = sink.events().await;
    let cap_event = events
        .iter()
        .find(|e| e.name == telemetry::tengu::workflow::BUDGET_CAP_EXCEEDED);
    assert!(
        cap_event.is_some(),
        "tengu_workflow_budget_cap_exceeded must fire; events: {events:?}"
    );
    let md = &cap_event.unwrap().metadata;
    assert!(
        matches!(md.get("spent"), Some(AnalyticsValue::Int(150))),
        "spent field must be 150"
    );
    assert!(
        matches!(md.get("budget"), Some(AnalyticsValue::Int(100))),
        "budget field must be 100"
    );
}

/// `tengu_workflow_agent_cap_exceeded` fires when the 1000-agent cap is hit.
#[tokio::test]
async fn telemetry_agent_cap_fires() {
    use telemetry::InMemorySink;
    let sink = Arc::new(InMemorySink::default());
    let bus = Arc::new(AnalyticsBus::new());
    bus.attach_sink(sink.clone()).await;

    let spawner = Arc::new(EchoSpawner::default());
    let _ = run_workflow_script(
        "for (let i = 0; i < 1001; i++) { await agent('x'); } return 'done';",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        bus.clone(),
        None,
        None,
    )
    .await; // expected Err

    let events: Vec<_> = sink.events().await;
    let cap_event = events
        .iter()
        .find(|e| e.name == telemetry::tengu::workflow::AGENT_CAP_EXCEEDED);
    assert!(
        cap_event.is_some(),
        "tengu_workflow_agent_cap_exceeded must fire; events: {events:?}"
    );
    let md = &cap_event.unwrap().metadata;
    assert!(
        matches!(md.get("agentCount"), Some(AnalyticsValue::Int(1000))),
        "agentCount field must be 1000"
    );
}

#[tokio::test]
async fn telemetry_launched_uses_declared_phase_count_and_utf16_script_size() {
    use telemetry::InMemorySink;
    let sink = Arc::new(InMemorySink::default());
    let bus = Arc::new(AnalyticsBus::new());
    bus.attach_sink(sink.clone()).await;
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let status = Arc::new(RecordingSink::default());
    let handler = LocalWorkflowHandler::new(
        Arc::new(EchoSpawner {
            total_tokens: 40,
            total_tool_use_count: 3,
            total_duration_ms: 7,
            ..Default::default()
        }),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        mgr,
    )
    .with_status_sink(status.clone())
    .with_bus(bus);
    let script = "export const meta = { name: 'named', description: 'd', phases: [{ title: 'A' }, { title: 'B' }] };\nphase('A'); await agent('a'); phase('B'); await agent('b'); return '😀';";
    let mut input = workflow_input(script);
    if let TaskSpawnInput::LocalWorkflow {
        invocation_mode,
        workflow_source,
        script_is_verbatim_builtin,
        ..
    } = &mut input
    {
        *invocation_mode = Some("named".into());
        *workflow_source = Some("built-in".into());
        *script_is_verbatim_builtin = Some(true);
    }
    handler.spawn(input, make_ctx(fs)).await.unwrap();
    assert_eq!(await_terminal(&status).await, TaskStatus::Completed);

    let events = sink.events().await;
    let launched = events
        .iter()
        .find(|event| event.name == telemetry::tengu::workflow::LAUNCHED)
        .expect("launched event");
    assert!(matches!(
        launched.metadata.get("phase_count"),
        Some(AnalyticsValue::Int(2))
    ));
    assert!(matches!(
        launched.metadata.get("script_size_chars"),
        Some(AnalyticsValue::Int(value)) if *value == workflow_script_size_chars(script)
    ));
    assert!(matches!(
        launched.metadata.get("workflow_name"),
        Some(AnalyticsValue::String(value)) if value == "named"
    ));
    assert!(matches!(
        launched.metadata.get("workflow_description"),
        Some(AnalyticsValue::String(value)) if value == "d"
    ));
    let completed = events
        .iter()
        .find(|event| event.name == telemetry::tengu::workflow::COMPLETED)
        .expect("completed event");
    assert!(matches!(
        completed.metadata.get("total_tokens"),
        Some(AnalyticsValue::Int(80))
    ));
    assert!(matches!(
        completed.metadata.get("total_tool_calls"),
        Some(AnalyticsValue::Int(6))
    ));
    let phase_events: Vec<_> = events
        .iter()
        .filter(|event| event.name == telemetry::tengu::workflow::PHASE_COMPLETED)
        .collect();
    assert_eq!(phase_events.len(), 2);
    assert!(matches!(
        phase_events[0].metadata.get("phase_index"),
        Some(AnalyticsValue::Int(1))
    ));
    assert!(matches!(
        phase_events[0].metadata.get("phase_tokens"),
        Some(AnalyticsValue::Int(40))
    ));
}

// ==== Structured progress event tests (Task 10) ==========================

/// A single-agent script emits `start` then `done` workflow_agent events to
/// the progress spool, with the correct index (0-based), label, state, and
/// toolUseID format (`workflow_agent_{index}_{suffix}`).
#[tokio::test]
async fn workflow_agent_progress_start_and_done_emitted() {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let spawner = Arc::new(EchoSpawner::default());
    run_workflow_script(
        "const r = await agent('analyze the code', { label: 'my-label' }); log('r=' + r);",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        Some(tx),
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .expect("runs");

    let mut lines: Vec<String> = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push(line);
    }

    // Must have at least a `start` and a `done` agent event, plus a log.
    let agent_lines: Vec<&str> = lines
        .iter()
        .filter(|l| l.starts_with("[workflow_agent]"))
        .map(|l| l.as_str())
        .collect();
    assert!(
        agent_lines.len() >= 2,
        "expected start+done events; got: {lines:?}"
    );

    // Parse the start event.
    let start_json_str = agent_lines[0].trim_start_matches("[workflow_agent] ");
    let start: serde_json::Value = serde_json::from_str(start_json_str).expect("start is JSON");
    assert_eq!(start["type"], "workflow_agent", "type field");
    assert_eq!(start["index"], 0, "index is 0 for first agent");
    assert_eq!(start["label"], "my-label", "label from opts.label");
    assert_eq!(start["state"], "start", "first event is start");
    let tool_use_id = start["toolUseID"].as_str().expect("toolUseID present");
    assert!(
        tool_use_id.starts_with("workflow_agent_0_"),
        "toolUseID format: {tool_use_id}"
    );

    // Parse the done event.
    let done_json_str = agent_lines[1].trim_start_matches("[workflow_agent] ");
    let done: serde_json::Value = serde_json::from_str(done_json_str).expect("done is JSON");
    assert_eq!(done["state"], "done", "second event is done");
    assert_eq!(done["index"], 0, "same agent index");
    assert!(done.get("agentId").is_some(), "done has agentId");
}

/// `phase()` calls produce `workflow_phase` formatted progress lines with index + title.
/// `log()` calls produce bare text lines (workflow_log style).
#[tokio::test]
async fn workflow_phase_and_log_progress_format() {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let spawner = Arc::new(EchoSpawner::default());
    run_workflow_script(
        "phase('Analysis'); log('hello world'); phase('Report');",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        Some(tx),
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .expect("runs");

    let mut lines: Vec<String> = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push(line);
    }

    // Phase lines render as "[index] === title ===".
    assert!(
        lines
            .iter()
            .any(|l| l.contains("[1]") && l.contains("=== Analysis ===")),
        "first phase: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("[2]") && l.contains("=== Report ===")),
        "second phase: {lines:?}"
    );
    // Log line renders as bare text.
    assert!(
        lines.iter().any(|l| l == "hello world"),
        "log line: {lines:?}"
    );
}

/// Agent index increments monotonically across sequential agent() calls.
#[tokio::test]
async fn workflow_agent_index_is_monotonic() {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let spawner = Arc::new(EchoSpawner::default());
    run_workflow_script(
        "await agent('first'); await agent('second'); await agent('third');",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        Some(tx),
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .expect("runs");

    let mut lines: Vec<String> = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push(line);
    }

    // Collect all start events and verify indices 0, 1, 2.
    let start_events: Vec<serde_json::Value> = lines
        .iter()
        .filter(|l| l.starts_with("[workflow_agent]"))
        .filter_map(|l| {
            let json_str = l.trim_start_matches("[workflow_agent] ");
            serde_json::from_str::<serde_json::Value>(json_str).ok()
        })
        .filter(|v| v["state"] == "start")
        .collect();
    assert_eq!(start_events.len(), 3, "three start events; got: {lines:?}");
    assert_eq!(start_events[0]["index"], 0);
    assert_eq!(start_events[1]["index"], 1);
    assert_eq!(start_events[2]["index"], 2);
}

/// cc 2.1.198 (M9): the workflow progress view keeps the EARLIEST agents
/// while the phase counter stays correct. The binary's fix
/// (`updateWorkflowProgressBatch`/`GCo` @213640399) keys `workflow_agent` /
/// `workflow_phase` rows on `${type}:${index}` and updates them in place —
/// when the row list overflows the window (`xVa=500` @213645694, trim at
/// `len > xVa*2`) ONLY `workflow_log` rows are dropped from the front, never
/// agent/phase rows. LingXi's progress stream is unbounded (spool + channel),
/// so earliest agents are retained by construction; this test locks that a
/// log flood past the binary's 1000-row trim threshold does not evict the
/// earliest agent or phase rows, and the phase indices stay correct.
#[tokio::test]
async fn workflow_progress_keeps_earliest_agents_through_log_flood() {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let spawner = Arc::new(EchoSpawner::default());
    // Earliest agents FIRST, then a >1000-line log flood (the cc trim
    // trigger), then a second phase with more agents.
    run_workflow_script(
        "phase('Early'); await agent('a0'); await agent('a1'); \
         for (let i = 0; i < 1100; i++) { log('flood ' + i); } \
         phase('Late'); await agent('a2');",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        Some(tx),
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .expect("runs");

    let mut lines: Vec<String> = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push(line);
    }

    let agent_events: Vec<serde_json::Value> = lines
        .iter()
        .filter(|l| l.starts_with("[workflow_agent]"))
        .filter_map(|l| {
            serde_json::from_str::<serde_json::Value>(l.trim_start_matches("[workflow_agent] "))
                .ok()
        })
        .collect();

    // The EARLIEST agents (indices 0 and 1, spawned before the flood) are
    // still present — both their start and done rows survive.
    for idx in [0, 1] {
        assert!(
            agent_events
                .iter()
                .any(|e| e["index"] == idx && e["state"] == "start"),
            "earliest agent {idx} start row retained; got {} agent events",
            agent_events.len()
        );
        assert!(
            agent_events
                .iter()
                .any(|e| e["index"] == idx && e["state"] == "done"),
            "earliest agent {idx} done row retained"
        );
    }
    // The post-flood agent is present too.
    assert!(
        agent_events
            .iter()
            .any(|e| e["index"] == 2 && e["state"] == "done"),
        "post-flood agent retained"
    );

    // The phase counter stays correct: both phase rows present with their
    // 1-based indices intact (earliest phase NOT dropped by the flood).
    assert!(
        lines
            .iter()
            .any(|l| l.contains("[1]") && l.contains("=== Early ===")),
        "earliest phase row retained with index 1"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("[2]") && l.contains("=== Late ===")),
        "second phase row retained with index 2"
    );
    // And the flood itself really crossed the binary's trim threshold.
    let flood_count = lines.iter().filter(|l| l.starts_with("flood ")).count();
    assert_eq!(flood_count, 1100, "the log flood was emitted in full");
}

/// Agent events include phaseIndex/phaseTitle when agent() is dispatched during a phase.
#[tokio::test]
async fn workflow_agent_carries_phase_context() {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let spawner = Arc::new(EchoSpawner::default());
    run_workflow_script(
        "phase('MyPhase'); await agent('task1');",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        Some(tx),
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .expect("runs");

    let mut lines: Vec<String> = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push(line);
    }

    let start_event = lines
        .iter()
        .filter(|l| l.starts_with("[workflow_agent]"))
        .find_map(|l| {
            let json_str = l.trim_start_matches("[workflow_agent] ");
            let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
            (v["state"] == "start").then_some(v)
        })
        .expect("start event present");

    assert_eq!(start_event["phaseIndex"], 1, "phaseIndex = 1 (first phase)");
    assert_eq!(start_event["phaseTitle"], "MyPhase", "phaseTitle = MyPhase");
}

/// A journal-replayed agent emits a `cached` workflow_agent event.
#[tokio::test]
async fn workflow_agent_cached_event_on_journal_replay() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let dir = tempdir().unwrap();
    let mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));

    let script = "const r = await agent('task'); log('r=' + r);";

    // Run 1: fresh — journal the result.
    let spawner1 = Arc::new(EchoSpawner::default());
    let sink1 = Arc::new(RecordingSink::default());
    let handle1 = make_handler(spawner1.clone(), mgr.clone(), sink1.clone())
        .spawn(workflow_input(script), make_ctx(fs.clone()))
        .await
        .unwrap();
    assert_eq!(await_terminal(&sink1).await, TaskStatus::Completed);
    let spool1 = dir.path().join(format!("{}.output", handle1.task_id));
    let out1 = mgr.read(&spool1, Default::default()).await.unwrap();
    let run_id = out1
        .content
        .lines()
        .find_map(|l| l.strip_prefix("runId: "))
        .expect("runId")
        .to_string();

    // Run 2: resume — agent replays from journal, should see `cached` event.
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let journal_path = dir.path().join(format!("workflow-{run_id}.json"));
    let journal_content = fs
        .read_file(journal_path.to_str().unwrap(), None, None)
        .await
        .unwrap()
        .content;
    let cache: std::collections::HashMap<String, String> =
        serde_json::from_str(&journal_content).expect("journal JSON");
    let journal = Arc::new(std::sync::Mutex::new(cache));

    run_workflow_script(
        script,
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        Arc::new(EchoSpawner::default()),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        Some(tx),
        Some(journal),
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .expect("resume runs");

    let mut lines: Vec<String> = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push(line);
    }

    let cached_event = lines
        .iter()
        .filter(|l| l.starts_with("[workflow_agent]"))
        .find_map(|l| {
            let json_str = l.trim_start_matches("[workflow_agent] ");
            let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
            (v["state"] == "cached").then_some(v)
        });
    assert!(
        cached_event.is_some(),
        "cached event must be emitted on journal replay; lines: {lines:?}"
    );
    let ev = cached_event.unwrap();
    assert_eq!(ev["index"], 0, "cached agent has index 0");
    let tuid = ev["toolUseID"].as_str().expect("toolUseID");
    assert_eq!(tuid, "workflow_agent_0_cached", "cached toolUseID format");
}

/// A failed agent emits an `error` workflow_agent event.
#[tokio::test]
async fn workflow_agent_error_event_on_failure() {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let spawner = Arc::new(EchoSpawner {
        fail: true,
        ..Default::default()
    });
    run_workflow_script(
        "const r = await agent('task'); log('r=' + r);",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        spawner,
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        Some(tx),
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .expect("runs (failed agent is not a script error)");

    let mut lines: Vec<String> = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push(line);
    }

    let error_event = lines
        .iter()
        .filter(|l| l.starts_with("[workflow_agent]"))
        .find_map(|l| {
            let json_str = l.trim_start_matches("[workflow_agent] ");
            let v: serde_json::Value = serde_json::from_str(json_str).ok()?;
            (v["state"] == "error").then_some(v)
        });
    let error_event = error_event.unwrap_or_else(|| {
        panic!("error event must be emitted for failed agent; lines: {lines:?}")
    });
    assert_eq!(
        error_event["error"], "boom",
        "the terminal event must preserve the subagent failure reason"
    );
}

/// Critical workflows can opt out of the compatibility `null` result and make
/// a failed agent reject with its original reason.
#[tokio::test]
async fn workflow_agent_throw_on_error_preserves_failure_reason() {
    let err = run_workflow_script(
        "await agent('task', { throwOnError: true });",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        Arc::new(EchoSpawner {
            fail: true,
            ..Default::default()
        }),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        Arc::new(AnalyticsBus::new()),
        None,
        None,
    )
    .await
    .expect_err("throwOnError must reject the agent promise");

    assert!(
        err.to_string().contains("boom"),
        "the workflow error must retain the real subagent reason: {err}"
    );
}

#[tokio::test]
async fn workflow_live_observer_uses_progress_state_and_surfaces_retry_attempt() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let observer = WorkflowAgentLiveObserver::new_with_metrics(
        None,
        Some(tx),
        workflow_progress_update(&workflow::Progress::Agent {
            index: 3,
            label: "Design".to_string(),
            phase_index: Some(1),
            phase_title: Some("Design".to_string()),
            agent_id: None,
            model: Some("test-model".to_string()),
            state: workflow::AgentState::Start,
            error: None,
            tool_use_id: "workflow_agent_3_queued".to_string(),
        }),
        None,
        None,
        0,
    );
    let agent_id = protocol::AgentId::new();
    let agent_id_string = agent_id.to_string();

    platform_api::subagent_spawn::SubagentSpawnObserver::on_event(
        &observer,
        platform_api::subagent_spawn::SubagentObservation::Allocated {
            agent_id,
            agent_type: "designer".to_string(),
            name: Some("Design agent".to_string()),
            model: "deepseek-flash".to_string(),
            model_profile: Some("deepseek".to_string()),
            persistent: false,
            initial_message_index: 0,
            origin_session_id: None,
        },
    )
    .await;
    let allocated = rx.recv().await.expect("allocated progress");
    assert_eq!(allocated.state.as_deref(), Some("progress"));
    assert_eq!(
        allocated.agent_id.as_deref(),
        Some(agent_id_string.as_str())
    );
    assert_eq!(allocated.agent_type.as_deref(), Some("designer"));
    assert_eq!(allocated.model.as_deref(), Some("deepseek/deepseek-flash"));

    platform_api::subagent_spawn::SubagentSpawnObserver::on_event(
        &observer,
        platform_api::subagent_spawn::SubagentObservation::Retry {
            agent_id,
            attempt: 2,
            reason: "workflow model query stalled while opening the response stream".to_string(),
        },
    )
    .await;
    let retry = rx.recv().await.expect("retry progress");
    assert_eq!(retry.state.as_deref(), Some("progress"));
    assert_eq!(retry.attempt, Some(2));
    assert!(retry
        .last_attempt_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("opening the response stream")));
}

#[tokio::test]
async fn workflow_live_observer_writes_rich_snapshots_to_spool() {
    let (spool_tx, mut spool_rx) = mpsc::unbounded_channel();
    let (live_tx, mut live_rx) = mpsc::unbounded_channel();
    emit_workflow_agent_queued(
        Some(&spool_tx),
        Some(&live_tx),
        4,
        "Design",
        "Design the flow",
        Some(2),
        Some("Implementation".to_string()),
        Some("deepseek/deepseek-flash".to_string()),
        100,
    );
    let queued_line = spool_rx.recv().await.expect("queued spool line");
    let queued_json: serde_json::Value =
        serde_json::from_str(queued_line.trim_start_matches("[workflow_agent] "))
            .expect("queued snapshot json");
    assert_eq!(queued_json["queuedAt"], 100);
    assert_eq!(queued_json["phaseIndex"], 2);
    assert_eq!(queued_json["toolUseID"], "workflow_agent_4_queued");
    let queued_live = live_rx.recv().await.expect("queued live update");
    assert_eq!(queued_live.queued_at_ms, Some(100));

    let observer = WorkflowAgentLiveObserver::new_with_metrics(
        Some(spool_tx),
        Some(live_tx),
        WorkflowProgressUpdate {
            kind: "workflow_agent".to_string(),
            index: 4,
            title: None,
            message: None,
            label: Some("Design".to_string()),
            phase_index: Some(2),
            phase_title: Some("Implementation".to_string()),
            agent_id: None,
            agent_type: Some("designer".to_string()),
            model: Some("deepseek/deepseek-flash".to_string()),
            fallback_model: None,
            state: Some("start".to_string()),
            error: None,
            tool_use_id: Some("workflow_agent_4_queued".to_string()),
            queued_at_ms: Some(100),
            started_at_ms: None,
            last_progress_at_ms: Some(100),
            attempt: Some(1),
            last_attempt_reason: None,
            tokens: None,
            tool_calls: None,
            last_tool_name: None,
            last_tool_summary: None,
            prompt_preview: Some("Design the flow".to_string()),
        },
        None,
        None,
        0,
    );
    let agent_id = protocol::AgentId::new();
    platform_api::subagent_spawn::SubagentSpawnObserver::on_event(
        &observer,
        platform_api::subagent_spawn::SubagentObservation::Allocated {
            agent_id,
            agent_type: "designer".to_string(),
            name: Some("Design agent".to_string()),
            model: "deepseek-flash".to_string(),
            model_profile: Some("deepseek".to_string()),
            persistent: false,
            initial_message_index: 0,
            origin_session_id: None,
        },
    )
    .await;
    platform_api::subagent_spawn::SubagentSpawnObserver::on_event(
        &observer,
        platform_api::subagent_spawn::SubagentObservation::Progress {
            agent_id,
            token_count: 11,
            tool_use_count: 2,
        },
    )
    .await;
    platform_api::subagent_spawn::SubagentSpawnObserver::on_event(
        &observer,
        platform_api::subagent_spawn::SubagentObservation::Completed {
            agent_id,
            content: Value::String("done".to_string()),
            total_tool_use_count: 3,
            total_duration_ms: 55,
            usage: SubagentUsage {
                input_tokens: 7,
                output_tokens: 5,
                ..Default::default()
            },
            assistant_message_count: 0,
            last_request_id: None,
        },
    )
    .await;

    let allocated_line = spool_rx.recv().await.expect("allocated spool line");
    let allocated_json: serde_json::Value =
        serde_json::from_str(allocated_line.trim_start_matches("[workflow_agent] "))
            .expect("allocated snapshot json");
    assert_eq!(allocated_json["state"], "progress");
    assert_eq!(
        allocated_json["startedAt"],
        allocated_json["lastProgressAt"]
    );

    let progress_line = spool_rx.recv().await.expect("progress spool line");
    let progress_json: serde_json::Value =
        serde_json::from_str(progress_line.trim_start_matches("[workflow_agent] "))
            .expect("progress snapshot json");
    assert_eq!(progress_json["tokens"], 11);
    assert_eq!(progress_json["toolCalls"], 2);

    let done_line = spool_rx.recv().await.expect("done spool line");
    let done_json: serde_json::Value =
        serde_json::from_str(done_line.trim_start_matches("[workflow_agent] "))
            .expect("done snapshot json");
    assert_eq!(done_json["state"], "done");
    assert_eq!(done_json["queuedAt"], 100);
    assert!(done_json.get("startedAt").is_some());
    assert_eq!(done_json["tokens"], 12);
    assert_eq!(done_json["toolCalls"], 3);

    for expected_tokens in [11_u64, 12_u64] {
        let update = live_rx.recv().await.expect("live observer update");
        if update.tokens == Some(expected_tokens) {
            if expected_tokens == 11 {
                assert_eq!(update.tool_calls, Some(2));
            } else {
                assert_eq!(update.tool_calls, Some(3));
                assert_eq!(update.state.as_deref(), Some("done"));
            }
        }
    }
}

/// r2-tests-honesty-005: nothing ties the `agentType: '<x>'` literals in the
/// Local App plugin's workflow scripts to the shipped agent roster
/// (`plugins/lingxi-local-app/agents/*.md`) -- neither `check-phase2-plugin.py`
/// (which only checks the roster directory listing, never opens a workflow
/// script) nor any Rust test. A workflow renamed to an `agentType` with no
/// `.md` on disk passed every existing gate. This test closes the Rust half:
/// it reads the live plugin workflow scripts and roster off disk (no
/// hand-typed name list to rot) and fails if extraction comes back empty
/// (fail-closed, matching the still-missing Python gate's intended posture)
/// or if any extracted `agentType` has no matching roster file.
#[test]
fn plugin_workflow_agent_types_match_shipped_agent_roster() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let plugin_root = manifest_dir
        .parent()
        .expect("tasks/ has a parent")
        .join("plugins")
        .join("lingxi-local-app");
    let workflows_dir = plugin_root.join("workflows");
    let agents_dir = plugin_root.join("agents");

    let roster: StdHashSet<String> = std::fs::read_dir(&agents_dir)
        .unwrap_or_else(|error| panic!("cannot read agent roster dir {agents_dir:?}: {error}"))
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .map(str::to_string)
            } else {
                None
            }
        })
        .collect();
    assert!(
        !roster.is_empty(),
        "agent roster dir {agents_dir:?} yielded zero .md files; needle extraction cannot be trusted"
    );

    let mut found_agent_types: StdHashSet<String> = StdHashSet::new();
    let workflow_scripts = std::fs::read_dir(&workflows_dir)
        .unwrap_or_else(|error| panic!("cannot read workflows dir {workflows_dir:?}: {error}"));
    for entry in workflow_scripts.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("js") {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read workflow script {path:?}: {error}"));
        let mut rest = source.as_str();
        while let Some(at) = rest.find("agentType:") {
            rest = &rest[at + "agentType:".len()..];
            let rest_trimmed = rest.trim_start();
            let quote = rest_trimmed
                .chars()
                .next()
                .filter(|char_| *char_ == '\'' || *char_ == '"');
            if let Some(quote) = quote {
                let after_quote = &rest_trimmed[1..];
                if let Some(end) = after_quote.find(quote) {
                    found_agent_types.insert(after_quote[..end].to_string());
                }
            }
            rest = rest_trimmed;
        }
    }
    assert!(
        !found_agent_types.is_empty(),
        "extracted zero `agentType:` literals from {workflows_dir:?}; the needle \
         derivation itself is broken (verify against a known sample before trusting a \
         zero-hit scan of any workflow script)"
    );

    let unrostered: Vec<&String> = found_agent_types
        .iter()
        .filter(|agent_type| !roster.contains(*agent_type))
        .collect();
    assert!(
        unrostered.is_empty(),
        "workflow scripts under {workflows_dir:?} reference agentType(s) {unrostered:?} \
         with no matching {agents_dir:?}/<name>.md in the shipped roster \
         (roster: {roster:?})"
    );
}

/// [R5-18] EVERY rejection `parse_workflow_fusion_request` derives from the
/// caller-supplied `opts` object must carry the marker the prelude's
/// `__wf_pump` (`workflow/src/lib.rs`) searches for when it sets
/// `err.name = "WorkflowFusionOptionError"` — not only the
/// `deny_unknown_fields` "unknown field" shape. `workflow_description.txt`
/// tells the model, without qualification, to `catch (e)` a `fusion()`
/// rejection and branch on that name, so a wrong-typed / out-of-range /
/// malformed option value that reached JS as a plain `Error` skipped the
/// script's recovery branch and aborted the whole workflow over a fully
/// recoverable mistake.
///
/// The negative case at the bottom is the other half of the contract: a
/// rejection that is NOT about the caller's options (host state — no parent
/// model) must NOT claim to be an option error, or a script's recovery
/// branch would swallow a condition retrying cannot fix.
#[test]
fn every_workflow_fusion_option_rejection_carries_the_prelude_option_marker() {
    const MARKER: &str = "Workflow fusion() received an unknown option";
    let executor: Arc<dyn FusionExecutor> = ImmediateFusionExecutor::new(
        FusionAgentSurface {
            enabled: true,
            ..FusionAgentSurface::default()
        },
        3,
        Ok(workflow_fusion_result()),
    );

    for (opts_json, why) in [
        (r#"{"nope":true}"#, "unknown option key"),
        (r#"{"maxPanel":300}"#, "out-of-range u8 option value"),
        (r#"{"maxPanel":"3"}"#, "wrong-typed number option"),
        (r#"{"partialOk":1}"#, "wrong-typed bool option"),
        (r#"{"dimensions":"speed"}"#, "wrong-typed array option"),
        (r#"{"preset":"sloppy"}"#, "unrecognized preset option value"),
        (
            r#"{"models":[{"profile":"","model":"gpt-5.4"}]}"#,
            "malformed models entry (empty profile)",
        ),
        (
            r#"{"models":[{"model":""}]}"#,
            "malformed models entry (empty model)",
        ),
        (r#"{"maxPanel":"#, "syntactically broken opts object"),
        // [R7-3/R7-7] The five rows below are `opts`-derived rejections the
        // R5-18 sweep MISSED: nothing in `parse_workflow_fusion_request`
        // looked at them, so they were raised much later inside
        // `executor.run` (`fusion::orchestrator::validate_request` ->
        // `platform_api::normalize_dimensions`, and
        // `fusion::model_resolver::resolve_custom`'s list-shape checks) and
        // reached the script through `wf_throw(&error.to_string())` with no
        // marker — `err.name` stayed the JS default "Error".
        (
            r#"{"dimensions":["Coverage"]}"#,
            "dimension that is not lowercase snake_case",
        ),
        (
            r#"{"dimensions":["provider"]}"#,
            "reserved identity-like dimension",
        ),
        (
            r#"{"dimensions":["d1","d2","d3","d4","d5","d6","d7","d8","d9","d10","d11","d12","d13"]}"#,
            "more than 12 dimensions",
        ),
        (
            r#"{"models":[{"model":"a"}]}"#,
            "models list below the 2-entry minimum",
        ),
        (
            r#"{"models":[{"model":"a"},{"profile":"openai","model":"a"}]}"#,
            "duplicate models entries (one spelling the parent profile explicitly)",
        ),
        (
            r#"{"models":[{"model":"a"},{"model":"b"},{"model":"c"}],"maxPanel":2}"#,
            "models list longer than the requested panel cap",
        ),
    ] {
        let Err(err) = parse_workflow_fusion_request(
            Some(&executor),
            "review this",
            opts_json,
            "wf_fusion",
            Some("gpt-5.4"),
            Some("openai"),
        ) else {
            panic!("{why}: `{opts_json}` must reject");
        };
        assert!(
            err.to_string().contains(MARKER),
            "{why}: `{opts_json}` rejected with `{err}`, which does not contain the \
             prelude marker `{MARKER}` — the script's \
             `e.name === \"WorkflowFusionOptionError\"` branch cannot fire"
        );
    }

    let host_err = parse_workflow_fusion_request(
        Some(&executor),
        "review this",
        "{}",
        "wf_fusion",
        None,
        None,
    )
    .expect_err("a missing parent model must reject");
    assert!(
        !host_err.to_string().contains(MARKER),
        "a host-state rejection must NOT be labelled an option error, got `{host_err}`"
    );

    // [R7-3/R7-7] The other half of the new checks: they must not turn a
    // request the orchestrator would have ACCEPTED into a parse-time
    // rejection. `{"model":"a"}`/`{"model":"b"}` are not in any catalog —
    // that lookup is host state and stays `executor.run`'s job.
    for (opts_json, why) in [
        ("{}", "no options at all"),
        (
            r#"{"models":[{"model":"a"},{"model":"b"}]}"#,
            "a well-formed 2-entry models list (unknown-model lookup is host state)",
        ),
        (
            r#"{"models":[{"model":"a"},{"model":"b"},{"model":"c"}],"maxPanel":4}"#,
            "a models list inside the requested panel cap",
        ),
        (
            r#"{"dimensions":["coverage","evidence_quality"]}"#,
            "well-formed snake_case dimensions",
        ),
        (
            r#"{"dimensions":[]}"#,
            "an empty dimensions list (defaults)",
        ),
    ] {
        parse_workflow_fusion_request(
            Some(&executor),
            "review this",
            opts_json,
            "wf_fusion",
            Some("gpt-5.4"),
            Some("openai"),
        )
        .unwrap_or_else(|err| panic!("{why}: `{opts_json}` must be accepted, got `{err}`"));
    }
}

/// [R7-3/R7-7] `parse_workflow_fusion_request`'s `opts.dimensions` gate must
/// accept and reject EXACTLY what the orchestrator's own
/// `validate_request` -> `platform_api::normalize_dimensions` does — the
/// oracle here is that shared function itself, not a copied message, so the
/// two cannot drift into either a rejection the orchestrator would have
/// allowed or a value that slips past the marker and reaches JS as a plain
/// `Error`.
#[test]
fn workflow_fusion_dimension_gate_matches_the_orchestrators_own_rule() {
    const MARKER: &str = "Workflow fusion() received an unknown option";
    let executor: Arc<dyn FusionExecutor> = ImmediateFusionExecutor::new(
        FusionAgentSurface {
            enabled: true,
            ..FusionAgentSurface::default()
        },
        3,
        Ok(workflow_fusion_result()),
    );

    for dimensions in [
        vec![],
        vec!["coverage".to_string()],
        vec!["coverage".to_string(), "evidence_quality".to_string()],
        vec!["Coverage".to_string()],
        vec!["evidence-quality".to_string()],
        vec!["_coverage".to_string()],
        vec!["provider".to_string()],
        vec!["model".to_string()],
        (1..=13).map(|i| format!("d{i}")).collect::<Vec<_>>(),
    ] {
        let orchestrator_rejects = platform_api::normalize_dimensions(dimensions.clone()).is_err();
        let opts_json = serde_json::json!({ "dimensions": dimensions.clone() }).to_string();
        let parsed = parse_workflow_fusion_request(
            Some(&executor),
            "review this",
            &opts_json,
            "wf_fusion",
            Some("gpt-5.4"),
            Some("openai"),
        );
        assert_eq!(
            parsed.is_err(),
            orchestrator_rejects,
            "dimensions {dimensions:?}: `normalize_dimensions` rejects = \
             {orchestrator_rejects}, but the workflow parse gate rejects = {}",
            parsed.is_err()
        );
        if let Err(err) = parsed {
            assert!(
                err.to_string().contains(MARKER),
                "dimensions {dimensions:?} rejected with `{err}`, which does not carry \
                 the prelude marker `{MARKER}`"
            );
        }
    }
}

/// [R7-3/R7-7] End-to-end through the real bridge and the real
/// `workflow/src/lib.rs` prelude: a script that writes the documented
/// `e.name === "WorkflowFusionOptionError"` recovery branch must see that
/// name for an `opts.dimensions` mistake. The stand-in executor is armed
/// with the ACTUAL error `fusion::orchestrator::validate_request` produces
/// for this input (sourced from `platform_api::normalize_dimensions`, not a
/// hand-copied string), so before the parse-time gate existed this test saw
/// `err.name === "Error"`; with the gate the executor is never reached at
/// all, which the zero-`seen` assertion pins.
#[tokio::test]
async fn workflow_fusion_option_error_name_reaches_a_script_for_a_bad_dimension() {
    let orchestrator_error = platform_api::normalize_dimensions(vec!["Coverage".to_string()])
        .expect_err("`Coverage` must be rejected by the shared dimension rule");
    let executor = ImmediateFusionExecutor::new(
        FusionAgentSurface {
            enabled: true,
            ..FusionAgentSurface::default()
        },
        3,
        Err(orchestrator_error),
    );
    let outcome = run_workflow_script_with_live_updates_and_fusion(
        "try { await fusion('review this', { dimensions: ['Coverage'] }); return 'RESOLVED'; } \
         catch (e) { return e.name; }",
        DEFAULT_WORKFLOW_SUBAGENT,
        "",
        Arc::new(EchoSpawner::default()),
        Arc::new(MockInvoker),
        Arc::new(MockBudget),
        None,
        None,
        None,
        None,
        None,
        None,
        0,
        NestedConfig::default(),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        CancellationToken::new(),
        Some(executor.clone()),
        Some("wf_fusion".into()),
        Some("gpt-5.4".into()),
        Some("openai".into()),
        None,
        Arc::new(AnalyticsBus::new()),
        None,
        None,
        None,
    )
    .await
    .expect("the script catches the rejection itself");

    let result = outcome.result.as_deref().unwrap_or_default().to_string();
    assert!(
        result.contains("WorkflowFusionOptionError"),
        "a bad `dimensions` option must reach the script as \
         `e.name === \"WorkflowFusionOptionError\"`, got `{result}`"
    );
    assert!(
        executor.seen.lock().unwrap().is_empty(),
        "a bad `dimensions` option must be rejected before dispatch, but the executor \
         saw {} request(s)",
        executor.seen.lock().unwrap().len()
    );
}

/// A value inside `int({min:1,max:256})` is taken verbatim, including the
/// boundaries.
#[test]
fn an_in_range_override_is_taken_verbatim() {
    assert_eq!(parse_concurrency_cap_override("1"), Some(1));
    assert_eq!(parse_concurrency_cap_override("32"), Some(32));
    assert_eq!(parse_concurrency_cap_override("256"), Some(256));
    // `.test(e.trim())` — surrounding whitespace is trimmed before the regex.
    assert_eq!(parse_concurrency_cap_override("  7  "), Some(7));
    // `/^[+-]?\d+$/` admits a leading plus.
    assert_eq!(parse_concurrency_cap_override("+7"), Some(7));
}

/// Every rejection path in the oracle's transform returns `undefined`, and
/// `?? ur` then takes the CPU default. Nothing is clamped and nothing throws —
/// a clamp would honour a request the user never made, and throwing would lose
/// the run over a typo.
#[test]
fn an_unusable_override_falls_back_rather_than_clamping_or_failing() {
    for raw in [
        "0",                       // below min:1
        "-1",                      // passes the regex, rejected by min
        "257",                     // above max:256
        "1000",                    // far above max
        "8.5",                     // not digitsOnly
        "1e3",                     // not digitsOnly
        "abc",                     // not a number at all
        "",                        // empty
        "   ",                     // whitespace only
        "+",                       // sign with no digits
        "99999999999999999999999", // finite in JS, still above max
    ] {
        assert_eq!(
            parse_concurrency_cap_override(raw),
            None,
            "{raw:?} must fall back to the default, not clamp into range"
        );
    }
}

/// The override is not merely parsed — it REPLACES the CPU default, in both
/// directions. Eight cores default to 6, so each case below is visibly not the
/// default.
#[test]
fn an_accepted_override_replaces_the_cpu_default() {
    assert_eq!(
        concurrency_cap_default(8),
        6,
        "premise: 8 cores default to 6"
    );
    assert_eq!(
        resolve_concurrency_cap(Some("64"), 8),
        64,
        "the override raises the gate above the 16 ceiling the default obeys"
    );
    assert_eq!(
        resolve_concurrency_cap(Some("1"), 8),
        1,
        "and lowers it below the floor of 2 the default obeys"
    );
}

/// With nothing usable to apply, the gate is the CPU default — the override
/// path must not leak a zero, a clamp, or a panic into the run.
#[test]
fn without_a_usable_override_the_gate_is_the_cpu_default() {
    assert_eq!(resolve_concurrency_cap(None, 8), 6);
    assert_eq!(resolve_concurrency_cap(Some("0"), 8), 6);
    assert_eq!(resolve_concurrency_cap(Some("257"), 8), 6);
    assert_eq!(resolve_concurrency_cap(Some("nonsense"), 8), 6);
}

/// The env var's spelling is the contract with the user; a typo here fails
/// silently and looks exactly like the feature not existing.
#[test]
fn the_override_env_var_is_spelled_the_way_claude_code_spells_it() {
    assert_eq!(
        WORKFLOW_MAX_CONCURRENT_AGENTS_ENV,
        "CLAUDE_CODE_WORKFLOW_MAX_CONCURRENT_AGENTS"
    );
}
