//! Goal proposals for a local mobile main session and its owned command queue.

use super::MobileConfig;
use crate::goal_proposal::{GoalProposalPolicy, SessionGoalProposalHost};
use client::adapter::ClientEventSink;
use client::protocol::events::ClientEvent;
use client::protocol::text::ClientTextDto;
use command_api::RegistrySlashDispatcher;
use lingxi_core::host::goal::ModelProposedGoals;
use lingxi_core::host::{OrchestratorHandle, SlashCommandDispatcher, SlashDispatchResult};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use orchestrator::{ConversationOrchestrator, OrchestratorError};
use std::path::PathBuf;
use std::sync::Arc;
use tool_ui::ask_user_question::{AskUserQuestionResolver, TuiBridgeResolver};

struct MobileGoalProposalPolicy {
    interactive_local: bool,
    config_home: PathBuf,
}

impl GoalProposalPolicy for MobileGoalProposalPolicy {
    fn is_interactive_local(&self) -> bool {
        self.interactive_local
    }

    fn setting(&self, _owner: &ConversationOrchestrator) -> ModelProposedGoals {
        // Mobile has no CLI or managed-settings layer. Read its real user
        // source directly: neither a project file nor the merged active view
        // can weaken this consent decision. An unreadable source requires ask.
        // The general layer stack skips file errors. Read this single trusted
        // source with its fallible reader so corruption cannot grant consent.
        match lingxi_core::settings::loader::read_settings_file(
            &self.config_home.join("settings.json"),
        ) {
            Ok(Some(settings)) => match settings.model_proposed_goals.as_deref() {
                Some("disabled") => ModelProposedGoals::Disabled,
                Some("alwaysAsk") => ModelProposedGoals::AlwaysAsk,
                _ => ModelProposedGoals::Auto,
            },
            Ok(None) => ModelProposedGoals::Auto,
            Err(_) => ModelProposedGoals::AlwaysAsk,
        }
    }
}

pub(super) fn mobile_goal_proposal_host(
    config: &MobileConfig,
    interactive_launch: bool,
    transport: Option<tokio::sync::mpsc::Sender<tool_ui::AskUserQuestionExchange>>,
    bus: Arc<telemetry::AnalyticsBus>,
) -> Arc<SessionGoalProposalHost> {
    let resolver = transport.map(|transport| {
        Arc::new(TuiBridgeResolver::for_goal_proposal(transport))
            as Arc<dyn AskUserQuestionResolver>
    });
    Arc::new(SessionGoalProposalHost::new(
        Arc::new(MobileGoalProposalPolicy {
            interactive_local: interactive_launch
                && !config.app_agent_host
                && !config.scheduler_controller,
            config_home: config.lingxi_home.clone(),
        }),
        resolver,
        bus,
    ))
}

/// Called only inside the mobile queue watcher's reserved ActiveTurn. The
/// ordinary mobile transition fence protects claim and slash publication;
/// the reserved permission owner then drives the resulting model turn.
pub(super) async fn run_queued_goal_proposal(
    owner: &Arc<ConversationOrchestrator>,
    dispatcher: &RegistrySlashDispatcher,
    command: msgqueue::QueuedCommand,
    cancel: tokio_util::sync::CancellationToken,
    sink: &dyn ClientEventSink,
    transition: Arc<tokio::sync::Mutex<()>>,
) -> Result<(), OrchestratorError> {
    // Keep ordinary mobile slash behavior intact. Only this queue-owned Goal
    // path lowers the accepted local injector into its kickoff model turn.
    let dispatcher = dispatcher.clone_shared().with_injected_messages_as_turns();
    let dispatch_guard = transition.lock().await;
    if cancel.is_cancelled() || !owner.claim_goal_proposal(&command.uuid).await {
        return Ok(());
    }
    let Some(raw) = command.text_projection().cloned() else {
        return Ok(());
    };
    let kind = if command.source == msgqueue::QueueSource::TaskCompletion {
        "task-notification"
    } else {
        "auto-continuation"
    };
    let origin = serde_json::json!({"kind": kind});
    let source_origin = Utf16JsonProjection::from_serde_value(&origin)
        .expect("a trusted Goal origin is valid JSON");
    // This scope is installed within the spawned task, since task-local state
    // does not propagate across Tokio spawn. A proposal supplies no human turn.
    orchestrator::native_user_message_source::with_queued_turn_authority(
        false,
        orchestrator::native_user_message_source::with_source(
            orchestrator::native_user_message_source::NativeUserMessageSource {
                message_origin: Some(source_origin.clone()),
                prompt_source: None,
            },
            orchestrator::mod_prompt_origin::with_origin(origin.clone(), async {
                let result = dispatcher.dispatch(raw, cancel.clone()).await;
                drop(dispatch_guard);
                match result {
                    SlashDispatchResult::RunAsTurn { prompt } => owner
                        .run_queued_prompt_batch(
                            vec![orchestrator::QueuedPromptInput {
                                text: prompt,
                                message_origin: Some(source_origin),
                                mod_origin: Some(origin),
                                is_meta: false,
                                prompt_ingress_kind: Some(orchestrator::PromptIngressKind::Other),
                                ..Default::default()
                            }],
                            cancel,
                        )
                        .await
                        .map(|_| ()),
                    SlashDispatchResult::Handled { display }
                    | SlashDispatchResult::Unknown { display, .. } => {
                        sink.emit(ClientEvent::SlashCommandResult {
                            turn_id: None,
                            display: ClientTextDto::from_code_units(
                                display.string_units("").unwrap_or_default(),
                            ),
                            is_error: false,
                        })
                        .await;
                        Ok(())
                    }
                    SlashDispatchResult::Failed { diagnostic } => Err(OrchestratorError::Internal(
                        diagnostic.to_json_string().unwrap_or_default(),
                    )),
                    _ => Err(OrchestratorError::Internal(
                        "a queued Goal command did not produce a local Goal result".into(),
                    )),
                }
            }),
        ),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use command_api::{CommandRegistry, builtins::GoalHandler};
    use lingxi_core::host::goal::GoalProposalHost;
    use lingxi_core::session::GoalOrigin;
    use orchestrator::test_support::{
        MockApiClient, MockOutputStream, MockStreamingApiClient, NoOpPermissionGate,
        StaticMemoryProvider, content_block_start_text, content_block_stop, message_delta_stop,
        message_start, message_stop, noop_hook_executor, text_delta,
    };
    use tokio::sync::{Mutex, RwLock, mpsc};
    use tokio_util::sync::CancellationToken;

    struct Queue(Arc<msgqueue::MessageQueueManager>);

    #[async_trait::async_trait]
    impl orchestrator::prompt::mid_turn_input::MidTurnInputSource for Queue {
        fn supports_goal_proposals(&self) -> bool {
            true
        }

        async fn enqueue_goal_proposal(
            &self,
            id: String,
            body: Utf16JsonProjection,
            origin: GoalOrigin,
            cancel: CancellationToken,
        ) -> Result<(), String> {
            self.0.enqueue_goal_proposal(id, body, origin, cancel).await;
            Ok(())
        }

        async fn take_mid_turn_input(&self) -> Option<Utf16JsonProjection> {
            None
        }
    }

    struct Sink;

    #[async_trait::async_trait]
    impl ClientEventSink for Sink {
        async fn emit(&self, _: ClientEvent) {}
    }

    #[derive(Default)]
    struct Activity(std::sync::Mutex<Vec<(bool, bool)>>);

    #[async_trait::async_trait]
    impl lingxi_core::host::task_notification::TaskNotificationProvider for Activity {
        fn update_shell_session_activity(&self, _: bool, busy: bool, user_interaction: bool) {
            self.0.lock().unwrap().push((busy, user_interaction));
        }
        async fn take_pending_task_notifications(
            &self,
        ) -> Vec<lingxi_core::host::task_registry::TaskNotification> {
            Vec::new()
        }
    }

    struct Fixture {
        owner: Arc<ConversationOrchestrator>,
        host: Arc<SessionGoalProposalHost>,
        queue: Arc<msgqueue::MessageQueueManager>,
        dispatcher: RegistrySlashDispatcher,
        requests: mpsc::Receiver<tool_ui::AskUserQuestionExchange>,
        api: Arc<MockStreamingApiClient>,
        activity: Arc<Activity>,
        root: tempfile::TempDir,
    }

    fn fixture(interactive: bool, app_agent: bool) -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let queue = Arc::new(msgqueue::MessageQueueManager::new());
        let activity = Arc::new(Activity::default());
        let api = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
            message_start("goal", "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, "working toward the goal"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ]]));
        let owner = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new_with_streaming(
                orchestrator::OrchestratorConfig {
                    interactive_session: interactive,
                    ..Default::default()
                },
                Arc::new(MockApiClient::new(vec![])),
                api.clone(),
                Arc::new(tool_api::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                root.path().to_path_buf(),
            )
            .with_workspace_trusted(true)
            .with_task_notifications(activity.clone())
            .with_mid_turn_input(Arc::new(Queue(queue.clone()))),
        );
        let mut registry = CommandRegistry::new();
        registry.register_builtin_handler(Arc::new(GoalHandler::new(owner.clone())));
        let dispatcher = RegistrySlashDispatcher::new(Arc::new(RwLock::new(registry)));
        let config = MobileConfig {
            cwd: root.path().to_path_buf(),
            lingxi_home: root.path().join("config"),
            app_agent_host: app_agent,
            ..Default::default()
        };
        std::fs::create_dir_all(&config.lingxi_home).unwrap();
        let (tx, requests) = mpsc::channel(4);
        let host = mobile_goal_proposal_host(
            &config,
            interactive,
            Some(tx),
            Arc::new(telemetry::AnalyticsBus::new()),
        );
        host.bind(&owner);
        Fixture {
            owner,
            host,
            queue,
            dispatcher,
            requests,
            api,
            activity,
            root,
        }
    }

    async fn dequeue(fixture: &Fixture) -> msgqueue::QueuedCommand {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(command) = fixture.queue.dequeue().await {
                    break command;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn mobile_goal_proposals_dispatch_approved_and_direct_without_human_authority() {
        for ask_user in [false, true] {
            let mut f = fixture(true, false);
            assert!(f.host.is_available());
            f.owner.set_active_goal(&"previous".into()).await;
            f.host.submit("mobile goal".into(), ask_user).await.unwrap();
            if ask_user {
                assert!(f.queue.peek(|_| true).await.is_none());
                let request = f.requests.recv().await.unwrap();
                assert_eq!(
                    request.timeout_secs, None,
                    "Goal approval must not time out into consent"
                );
                assert!(request.lifetime.is_live_goal_proposal());
                request
                    .resp_tx
                    .send(vec![(
                        request.questions[0].question.clone(),
                        "Approve".into(),
                    )])
                    .unwrap();
            } else {
                assert!(f.requests.try_recv().is_err());
            }
            let command = dequeue(&f).await;
            assert!(command.is_slash_command());
            assert_eq!(
                f.owner
                    .get_active_goal()
                    .await
                    .unwrap()
                    .condition
                    .string_at(""),
                Some("previous")
            );
            let replay = command.clone();
            let owner = f.owner.clone();
            let dispatcher = f.dispatcher.clone_shared();
            // A spawn does not inherit task locals. The actual mobile helper
            // must stamp its own nonhuman source even under a human caller.
            orchestrator::native_user_message_source::with_source(
                orchestrator::native_user_message_source::human(),
                async move {
                    tokio::spawn(async move {
                        run_queued_goal_proposal(
                            &owner,
                            &dispatcher,
                            command,
                            CancellationToken::new(),
                            &Sink,
                            Arc::new(Mutex::new(())),
                        )
                        .await
                    })
                    .await
                    .unwrap()
                    .unwrap();
                },
            )
            .await;
            assert_eq!(
                f.owner
                    .get_active_goal()
                    .await
                    .unwrap()
                    .condition
                    .string_at(""),
                Some("mobile goal")
            );
            assert_eq!(f.api.captured_calls().await.len(), 1);
            let activity = f.activity.0.lock().unwrap().clone();
            assert!(
                activity.iter().any(|(busy, _)| *busy),
                "the real core model turn ran"
            );
            assert!(
                activity
                    .iter()
                    .all(|(_, user_interaction)| !user_interaction),
                "a Goal kickoff must not gain human lifecycle authority"
            );
            let expected_origin = if ask_user {
                "auto-continuation"
            } else {
                "task-notification"
            };
            let session = f.owner.session();
            assert!(session.lock().await.history.iter().any(|message| {
                matches!(message, lingxi_core::types::ConversationMessage::User { origin: Some(origin), .. }
                    if origin.string_at("/kind") == Some(expected_origin))
            }));
            run_queued_goal_proposal(
                &f.owner,
                &f.dispatcher,
                replay,
                CancellationToken::new(),
                &Sink,
                Arc::new(Mutex::new(())),
            )
            .await
            .unwrap();
            assert_eq!(
                f.api.captured_calls().await.len(),
                1,
                "a claimed kickoff cannot run twice"
            );
        }
    }

    #[tokio::test]
    async fn mobile_goal_proposal_dequeued_before_host_teardown_stays_retired() {
        let f = fixture(true, false);
        f.host.submit("stale goal".into(), false).await.unwrap();
        let command = dequeue(&f).await;
        f.owner.close_background_producer_admission();
        assert!(!f.host.is_available());
        run_queued_goal_proposal(
            &f.owner,
            &f.dispatcher,
            command,
            CancellationToken::new(),
            &Sink,
            Arc::new(Mutex::new(())),
        )
        .await
        .unwrap();
        assert!(f.owner.get_active_goal().await.is_none());
        assert!(f.api.captured_calls().await.is_empty());
    }

    #[tokio::test]
    async fn mobile_goal_proposal_admission_excludes_headless_and_app_agents() {
        for (interactive, app_agent) in [(false, false), (true, true)] {
            let f = fixture(interactive, app_agent);
            assert!(!f.host.is_available());
            assert!(!f.host.state().await.interactive_local);
        }
    }

    #[tokio::test]
    async fn mobile_goal_proposal_consent_uses_live_user_source_only() {
        let f = fixture(true, false);
        let user_path = f.root.path().join("config/settings.json");
        assert_eq!(f.host.state().await.setting, ModelProposedGoals::Auto);
        std::fs::write(&user_path, r#"{"modelProposedGoals":"auto"}"#).unwrap();
        assert_eq!(f.host.state().await.setting, ModelProposedGoals::Auto);
        std::fs::write(&user_path, r#"{"modelProposedGoals":"alwaysAsk"}"#).unwrap();
        let project_path = lingxi_core::settings::loader::project_settings_path(f.root.path());
        std::fs::create_dir_all(project_path.parent().unwrap()).unwrap();
        std::fs::write(project_path, r#"{"modelProposedGoals":"auto"}"#).unwrap();
        assert_eq!(f.host.state().await.setting, ModelProposedGoals::AlwaysAsk);
        std::fs::write(&user_path, r#"{"modelProposedGoals":"disabled"}"#).unwrap();
        assert!(!f.host.is_available());
        std::fs::write(&user_path, "{ broken").unwrap();
        assert_eq!(f.host.state().await.setting, ModelProposedGoals::AlwaysAsk);
        std::fs::write(&user_path, r#"{"modelProposedGoals":true}"#).unwrap();
        assert_eq!(f.host.state().await.setting, ModelProposedGoals::AlwaysAsk);
        std::fs::remove_file(&user_path).unwrap();
        std::fs::create_dir(&user_path).unwrap();
        assert_eq!(f.host.state().await.setting, ModelProposedGoals::AlwaysAsk);
    }

    #[derive(Default)]
    struct RecordingSink(std::sync::Mutex<Vec<ClientEvent>>);

    #[async_trait::async_trait]
    impl ClientEventSink for RecordingSink {
        async fn emit(&self, event: ClientEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    #[tokio::test]
    async fn mobile_goal_question_is_session_owned_and_resolves_on_reset_or_teardown() {
        for teardown in [false, true] {
            let mut f = fixture(true, false);
            let sink = Arc::new(RecordingSink::default());
            let broker = Arc::new(client::adapter::AskUserQuestionBroker::new(sink.clone()));
            let receiver = std::mem::replace(&mut f.requests, mpsc::channel(1).1);
            let running = broker.clone();
            let broker_task = tokio::spawn(async move { running.run(receiver).await });
            f.host
                .submit("awaiting approval".into(), true)
                .await
                .unwrap();
            let request_id = tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    let request = sink.0.lock().unwrap().iter().find_map(|event| {
                        if let ClientEvent::AskUserQuestion { request } = event {
                            assert!(request.session_owned);
                            Some(request.request_id)
                        } else {
                            None
                        }
                    });
                    if let Some(id) = request {
                        break id;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(broker.drain_turn().await, 0);
            assert!(broker.is_live_goal_proposal(request_id).await);
            if teardown {
                f.owner.close_background_producer_admission();
                assert!(!f.host.is_available());
            } else {
                f.owner.clear_session().await.unwrap();
            }
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while broker.pending_count().await != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(sink.0.lock().unwrap().iter().any(|event| {
                matches!(event, ClientEvent::AskUserQuestionResolved { request_id: id } if *id == request_id)
            }));
            assert!(f.queue.peek(|_| true).await.is_none());
            assert!(f.owner.get_active_goal().await.is_none());
            broker_task.abort();
            let _ = broker_task.await;
        }
    }
}
