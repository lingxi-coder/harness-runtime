//! Real pool and task-registry restoration; the model and review verdict are
//! controlled while startup, dispatch, peer admission and persistence are live.

use super::*;
use futures_util::StreamExt;
use lingxi_core::host::handback::*;
use lingxi_core::host::permission_gate::*;
use lingxi_core::types::{AgentId, ConversationMessage, MessageId, SessionId};
use std::sync::{Arc, Mutex};

struct MainInbox {
    scope: HandbackSessionScope,
    reports: Mutex<Vec<HandbackEnvelope>>,
}

#[async_trait]
impl ReportingAdmission for MainInbox {
    async fn main_scope(&self) -> Option<HandbackSessionScope> {
        Some(self.scope)
    }
    async fn admit(&self, envelope: HandbackEnvelope) -> Result<(), HandbackAdmissionError> {
        self.reports.lock().unwrap().push(envelope);
        Ok(())
    }
}

struct ReviewGate;
#[async_trait]
impl PermissionGate for ReviewGate {
    fn permission_mode(&self) -> Option<String> {
        Some("auto".into())
    }
    async fn check(&self, _: &str, _: &serde_json::Value) -> PermissionDecision {
        PermissionDecision::Allow
    }
    async fn check_classifier_only_with_context_or_abort(
        &self,
        name: &str,
        _: &serde_json::Value,
        _: &PermissionCheckContext,
        _: ClassifierOnlyPolicy,
        _: &ClassifierOnlyReviewRequest,
    ) -> Result<ClassifierOnlyOutcome, PermissionAbort> {
        assert_eq!(name, HANDBACK_TOOL_NAME);
        Ok(ClassifierOnlyOutcome {
            permission: PermissionOutcome::Allow {
                updated_input: None,
                permission_updates: Vec::new(),
                decision_classification: None,
            },
            review: Some(ReportReview::Passed),
        })
    }
}

struct Model {
    calls: Mutex<Vec<Vec<ConversationMessage>>>,
    release_caller: tokio::sync::Notify,
    caller_calls: std::sync::atomic::AtomicUsize,
}

fn response(content: llm_runtime::ContentBlock, stop: &str) -> llm_runtime::HistoryResponse {
    llm_runtime::HistoryResponse {
        id: "restored-response".into(),
        model: "claude-opus-5".into(),
        content: vec![content],
        stop_reason: Some(stop.into()),
        stop_details: None,
        usage: Default::default(),
        cost: None,
        provider_metadata: serde_json::Value::Null,
    }
}

#[async_trait]
impl agent::SubagentApiClient for Model {
    async fn stream(
        &self,
        request: agent::api::SubagentApiRequest,
    ) -> Result<
        futures_util::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let messages = request.messages;
        let tools = request.tools;
        let response: Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> = {
            let caller = serde_json::to_string(&messages)
                .unwrap()
                .contains("original caller context");
            self.calls.lock().unwrap().push(messages);
            if caller {
                if self
                    .caller_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    == 0
                {
                    self.release_caller.notified().await;
                }
                Ok(response(
                    llm_runtime::ContentBlock::Text {
                        text: "caller finished".into(),
                        cache_control: None, citations: None,
                    },
                    "end_turn",
                ))
            } else {
                assert!(tools.iter().any(|tool| tool["name"] == HANDBACK_TOOL_NAME));
                Ok(response(
                    llm_runtime::ContentBlock::ToolCall {
                        id: "restored-handback".into(),
                        name: HANDBACK_TOOL_NAME.into(),
                        input: serde_json::json!({"message":"fresh report from restored child"}),
                    },
                    "tool_use",
                ))
            }
        };
        let events = llm_runtime::stream_accumulator::response_to_stream_events(response?);
        Ok(futures_util::stream::iter(events.into_iter().map(Ok)).boxed())
    }
}

struct Budget;
#[async_trait]
impl lingxi_core::host::BudgetEnforcerHandle for Budget {
    async fn check_and_charge(&self, _: u64) -> Result<(), lingxi_core::host::BudgetError> {
        Ok(())
    }
    async fn snapshot_total_nano_usd(&self) -> u64 {
        0
    }
}

struct ResumeGate;
#[async_trait]
impl ForkResumeGate for ResumeGate {
    async fn check_resume(&self, _: AgentId, _: Option<&str>) -> Result<(), String> {
        Ok(())
    }
}

async fn startup_diagnostics(
    registry: &tasks::registry::TaskRegistry,
    aliases: &[String],
    model: &Model,
    main: &MainInbox,
) -> String {
    let mut rows = Vec::new();
    for alias in aliases {
        let task_id = registry.resolve_task_id(alias).await;
        let state = match task_id.as_deref() {
            Some(task_id) => registry.get(task_id).await,
            None => None,
        };
        let output = match state.as_ref() {
            Some(state) => tokio::fs::read_to_string(&state.base().output_file)
                .await
                .unwrap_or_else(|error| format!("unreadable output: {error}")),
            None => "no task row".into(),
        };
        rows.push((alias, task_id, state, output));
    }
    format!(
        "rows={rows:#?}; model_calls={}; admitted_reports={}",
        model.calls.lock().unwrap().len(),
        main.reports.lock().unwrap().len(),
    )
}

#[allow(clippy::too_many_arguments)]
async fn persisted_agent(
    directory: &std::path::Path,
    id: AgentId,
    task_id: &str,
    context: &str,
    creator: Option<AgentId>,
    state: HandbackState,
    opt_in: bool,
    archive: Vec<HandbackState>,
) {
    session::agent_rows::write_row(
        directory,
        &session::agent_rows::ParkedAgentRow {
            task_id: task_id.into(),
            agent_id: id,
            description: context.into(),
            request: SubagentSpawnRequest {
                subagent_type: "general-purpose".into(),
                prompt: "original prompt must not replay".into(),
                model: Some("claude-opus-5".into()),
                run_in_background: true,
                origin_session_id: Some(state.run.scope.session_id),
                creator_agent_id: creator,
                ..Default::default()
            },
            handback_opt_in: opt_in,
            handback_state: Some(state),
            handback_history: archive,
        },
    )
    .await
    .unwrap();
    let rows = [
        ConversationMessage::user(MessageId::new(), context.into()),
        ConversationMessage::user_meta(
            MessageId::new(),
            format!("<system-reminder>\n{HANDBACK_REMINDER}\n</system-reminder>"),
        ),
    ];
    let body = rows
        .iter()
        .map(|message| format!("{}\n", serde_json::json!({"agent_id":id,"message":message})))
        .collect::<String>();
    tokio::fs::write(
        session::forked_skill::agent_transcript_path(directory, &id.to_string()),
        body,
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actual_cold_batch_retains_later_caller_and_inactive_archive_without_reusing_receipt() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("agents");
    let session_id = SessionId::new();
    let old = HandbackSessionScope {
        session_id,
        activation_epoch: 1,
    };
    let current = HandbackSessionScope {
        session_id,
        activation_epoch: 9,
    };
    let mut actors = [AgentId::new(), AgentId::new()];
    actors.sort_by_key(std::string::ToString::to_string);
    let [child, caller] = actors;
    let creator = AgentId::new();
    let old_state = |id, recipient, active| {
        let run = HandbackRunKey {
            scope: old,
            agent_id: id,
            run_epoch: 3,
        };
        let mut state =
            HandbackState::new_run(run, active, None, Some(recipient), recipient, |_| true);
        state.receipt = Some(HandbackReceipt {
            run,
            recipient,
            message_id: MessageId::new(),
        });
        state.report = Some(HandbackReport {
            text: "old admitted whole report".into(),
            warning: Some("old warning".into()),
        });
        state.disposition = Some(HandbackDisposition::Flagged);
        state.bounce_count = 3;
        state
    };
    let child_state = old_state(
        child,
        HandbackRecipient::Agent {
            scope: old,
            agent_id: caller,
        },
        true,
    );
    let caller_state = old_state(caller, HandbackRecipient::Main { scope: old }, false);
    persisted_agent(
        &directory,
        child,
        "old-child",
        "original child context",
        Some(creator),
        child_state,
        true,
        Vec::new(),
    )
    .await;
    persisted_agent(
        &directory,
        caller,
        "old-caller",
        "original caller context",
        None,
        caller_state.clone(),
        false,
        vec![caller_state.clone()],
    )
    .await;
    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix_minimal::PosixFileSystem::new(root.path().to_path_buf()),
    );
    let runtime: Arc<dyn lingxi_core::host::RuntimeSpawner> =
        Arc::new(platform_posix_minimal::PosixRuntime::new());
    let model = Arc::new(Model {
        calls: Mutex::new(Vec::new()),
        release_caller: Default::default(),
        caller_calls: Default::default(),
    });
    let pool = Arc::new(agent::StateMachinePool::new(runtime.clone(), 4));
    let spawner = Arc::new(
        agent::PoolSubagentSpawner::new(pool)
            .with_api_client(model.clone())
            .with_permission_mode(permission::PermissionMode::Auto)
            .with_hook_context(
                session_id,
                root.path().to_path_buf(),
                Some(directory.clone()),
            )
            .with_transcript_fs(fs.clone()),
    );
    let tools = Arc::new(tool_api::ToolRegistry::new());
    let _ = spawner.tool_registry_handle().set(tools.clone());
    let invoker: Arc<dyn lingxi_core::host::ToolInvoker> =
        Arc::new(tool_api::RegistryToolInvoker::new(tools).with_gate(Arc::new(ReviewGate)));
    let budget: Arc<dyn lingxi_core::host::BudgetEnforcerHandle> = Arc::new(Budget);
    let output_dir = root.path().join("output");
    tokio::fs::create_dir_all(&output_dir).await.unwrap();
    let output = Arc::new(tasks::output_manager::TaskOutputManager::new(
        output_dir,
        fs.clone(),
    ));
    let sink = Arc::new(tasks::registry_status_sink::RegistryStatusSink::new());
    let mut registry = tasks::registry::TaskRegistry::new(runtime, fs, output.clone());
    registry.register_handler(
        tasks::TaskType::LocalAgent,
        Arc::new(
            tasks::handlers::LocalAgentHandler::new(
                spawner.clone(),
                invoker.clone(),
                budget.clone(),
                output,
            )
            .with_streaming_spawner(spawner.clone())
            .with_status_sink(sink.clone())
            .with_parked_agent_store(Arc::new(FileParkedAgentStore {
                subagents_dir: directory.clone(),
            })),
        ),
    );
    let registry = Arc::new(registry);
    sink.bind(registry.clone());
    spawner.set_task_registry(registry.clone());
    let main = Arc::new(MainInbox {
        scope: current,
        reports: Mutex::new(Vec::new()),
    });
    registry.bind_reporting_admission(Arc::downgrade(
        &(main.clone() as Arc<dyn ReportingAdmission>),
    ));
    let inheritance = SubagentInheritance {
        tool_invoker: invoker,
        budget,
    };
    let outcomes = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        restore_parked_agents_in_registry(
            &directory,
            registry.clone(),
            &ResumeGate,
            &inheritance,
            Some("auto".into()),
        ),
    )
    .await
    .unwrap();
    let aliases = vec!["old-child".into(), "old-caller".into()];
    assert!(
        outcomes
            .iter()
            .all(|(_, outcome)| matches!(outcome, RestoreOutcome::Restored(_))),
        "actual restore launch failed: {outcomes:#?}; {}",
        startup_diagnostics(&registry, &aliases, &model, &main).await,
    );
    let admitted = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if registry
                .handback_state_for_agent(child)
                .await
                .is_some_and(|state| state.receipt.is_some())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        admitted.is_ok(),
        "fresh report was not admitted; {}",
        startup_diagnostics(&registry, &aliases, &model, &main).await
    );
    let fresh = registry.handback_state_for_agent(child).await.unwrap();
    assert_eq!(
        fresh.recipient,
        HandbackRecipient::Agent {
            scope: current,
            agent_id: caller
        }
    );
    assert_eq!(fresh.run.run_epoch, 4);
    assert_eq!(fresh.disposition, Some(HandbackDisposition::Send));
    assert_eq!(
        fresh.report.as_ref().unwrap().text,
        "fresh report from restored child"
    );
    assert!(
        main.reports.lock().unwrap().is_empty(),
        "later readable caller must not fall back to main"
    );
    let child_task_id = registry.resolve_task_id("old-child").await.unwrap();
    let child_row = registry.get(&child_task_id).await.unwrap();
    assert_eq!(child_row.base().creator_agent_id, Some(creator));
    let inactive = registry.handback_state_for_agent(caller).await.unwrap();
    assert!(!inactive.active && inactive.receipt.is_none() && inactive.report.is_none());
    assert_eq!(inactive.run.scope, current);
    assert_eq!(inactive.run.run_epoch, caller_state.run.run_epoch + 1);
    assert_eq!(
        inactive.recipient,
        HandbackRecipient::Main { scope: current }
    );
    assert_eq!(inactive.fallback_main, current);
    assert_eq!(inactive.bounce_count, 0);
    assert_eq!(inactive.disposition, None);
    model.release_caller.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Some(row) = session::agent_rows::read_row(&directory, &caller.to_string()).await
            {
                if row
                    .handback_state
                    .as_ref()
                    .is_some_and(|state| state.run.run_epoch == 4)
                {
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let persisted = session::agent_rows::read_row(&directory, &caller.to_string())
        .await
        .unwrap();
    assert!(persisted
        .handback_state
        .as_ref()
        .is_some_and(|state| !state.active && state.report.is_none()));
    assert!(
        persisted.handback_history.iter().any(|state| state == &caller_state),
        "cold resume must archive the full original report, warning, receipt and recipient scope unchanged"
    );
    {
        let calls = model.calls.lock().unwrap();
        assert!(calls
            .iter()
            .filter(|messages| serde_json::to_string(messages)
                .unwrap()
                .contains("original caller context"))
            .any(|messages| serde_json::to_string(messages)
                .unwrap()
                .contains(HANDBACK_COUNTERMAND)));
    }
    registry.shutdown_background_tasks().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actual_cold_batch_releases_async_pool_full_actor_before_waiting_peer_starts_model() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("agents");
    let session_id = SessionId::new();
    let old = HandbackSessionScope {
        session_id,
        activation_epoch: 1,
    };
    let current = HandbackSessionScope {
        session_id,
        activation_epoch: 2,
    };
    let actors = [AgentId::new(), AgentId::new(), AgentId::new()];
    for (index, id) in actors.iter().enumerate() {
        let state = HandbackState::new_run(
            HandbackRunKey {
                scope: old,
                agent_id: *id,
                run_epoch: 1,
            },
            true,
            None,
            None,
            HandbackRecipient::Main { scope: old },
            |_| true,
        );
        persisted_agent(
            &directory,
            *id,
            &format!("old-actor-{index}"),
            &format!("cold actor {index}"),
            None,
            state,
            true,
            Vec::new(),
        )
        .await;
    }
    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix_minimal::PosixFileSystem::new(root.path().to_path_buf()),
    );
    let runtime: Arc<dyn lingxi_core::host::RuntimeSpawner> =
        Arc::new(platform_posix_minimal::PosixRuntime::new());
    let model = Arc::new(Model {
        calls: Mutex::new(Vec::new()),
        release_caller: Default::default(),
        caller_calls: Default::default(),
    });
    let pool = Arc::new(agent::StateMachinePool::new(runtime.clone(), 1));
    let spawner = Arc::new(
        agent::PoolSubagentSpawner::new(pool)
            .with_api_client(model.clone())
            .with_permission_mode(permission::PermissionMode::Auto)
            .with_hook_context(
                session_id,
                root.path().to_path_buf(),
                Some(directory.clone()),
            )
            .with_transcript_fs(fs.clone()),
    );
    let tools = Arc::new(tool_api::ToolRegistry::new());
    let _ = spawner.tool_registry_handle().set(tools.clone());
    let invoker: Arc<dyn lingxi_core::host::ToolInvoker> =
        Arc::new(tool_api::RegistryToolInvoker::new(tools).with_gate(Arc::new(ReviewGate)));
    let budget: Arc<dyn lingxi_core::host::BudgetEnforcerHandle> = Arc::new(Budget);
    let output_dir = root.path().join("output");
    tokio::fs::create_dir_all(&output_dir).await.unwrap();
    let output = Arc::new(tasks::output_manager::TaskOutputManager::new(
        output_dir,
        fs.clone(),
    ));
    let sink = Arc::new(tasks::registry_status_sink::RegistryStatusSink::new());
    let mut registry = tasks::registry::TaskRegistry::new(runtime, fs, output.clone());
    registry.register_handler(
        tasks::TaskType::LocalAgent,
        Arc::new(
            tasks::handlers::LocalAgentHandler::new(
                spawner.clone(),
                invoker.clone(),
                budget.clone(),
                output,
            )
            .with_streaming_spawner(spawner.clone())
            .with_status_sink(sink.clone())
            .with_parked_agent_store(Arc::new(FileParkedAgentStore {
                subagents_dir: directory.clone(),
            })),
        ),
    );
    let registry = Arc::new(registry);
    sink.bind(registry.clone());
    spawner.set_task_registry(registry.clone());
    let main = Arc::new(MainInbox {
        scope: current,
        reports: Mutex::new(Vec::new()),
    });
    registry.bind_reporting_admission(Arc::downgrade(
        &(main.clone() as Arc<dyn ReportingAdmission>),
    ));
    let inheritance = SubagentInheritance {
        tool_invoker: invoker,
        budget,
    };
    let outcomes = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        restore_parked_agents_in_registry(
            &directory,
            registry.clone(),
            &ResumeGate,
            &inheritance,
            Some("auto".into()),
        ),
    )
    .await
    .unwrap();
    let aliases: Vec<String> = (0..actors.len())
        .map(|index| format!("old-actor-{index}"))
        .collect();
    assert!(
        outcomes
            .iter()
            .all(|(_, outcome)| matches!(outcome, RestoreOutcome::Restored(_))),
        "capacity failure must occur in asynchronous startup, launch outcomes={outcomes:#?}; {}",
        startup_diagnostics(&registry, &aliases, &model, &main).await
    );
    let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let mut failed = 0;
            for index in 0..actors.len() {
                if let Some(task_id) = registry
                    .resolve_task_id(&format!("old-actor-{index}"))
                    .await
                {
                    if registry
                        .get(&task_id)
                        .await
                        .is_some_and(|state| state.base().status == tasks::TaskStatus::Failed)
                    {
                        failed += 1;
                    }
                }
            }
            if failed == 2 && main.reports.lock().unwrap().len() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(settled.is_ok(), "failed background startup must release its batch participant even while the resume recipe remains registered; {}",
        startup_diagnostics(&registry, &aliases, &model, &main).await);
    assert_eq!(
        model.calls.lock().unwrap().len(),
        1,
        "accepted cold runner reaches its real model API"
    );
    registry.shutdown_background_tasks().await.unwrap();
}
