//! Desktop policy composition for the shared nonblocking Goal proposal host.

use super::{DesktopConfig, load_effective_settings_for_config};
use crate::goal_proposal::{GoalProposalPolicy, SessionGoalProposalHost, trusted_setting};
use async_trait::async_trait;
use lingxi_core::host::goal::{GoalProposalHost, GoalProposalState, ModelProposedGoals};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use orchestrator::ConversationOrchestrator;
use std::sync::Arc;
use tool_ui::ask_user_question::AskUserQuestionResolver;
#[cfg(test)]
use {
    lingxi_core::{
        host::OrchestratorHandle,
        session::GoalOrigin,
        settings::{EffectiveSettings, tracer::Source},
    },
    std::sync::Mutex,
    tool_ui::ask_user_question::Question,
};

struct DesktopGoalProposalPolicy(DesktopConfig);

impl GoalProposalPolicy for DesktopGoalProposalPolicy {
    fn is_interactive_local(&self) -> bool {
        self.0.session_composition().is_interactive_session()
            && self.0.app_invocation.is_none()
            && self.0.team_invocation.is_none()
    }

    fn setting(&self, owner: &ConversationOrchestrator) -> ModelProposedGoals {
        let mut config = self.0.clone();
        config.cwd = owner.current_cwd();
        load_effective_settings_for_config(&config, None)
            .as_ref()
            .map(trusted_setting)
            .unwrap_or(ModelProposedGoals::AlwaysAsk)
    }
}

pub(super) struct DesktopGoalProposalHost {
    #[cfg(test)]
    config: DesktopConfig,
    inner: SessionGoalProposalHost,
}

impl DesktopGoalProposalHost {
    pub(super) fn new(
        config: DesktopConfig,
        resolver: Option<Arc<dyn AskUserQuestionResolver>>,
        bus: Arc<telemetry::AnalyticsBus>,
    ) -> Self {
        Self {
            inner: SessionGoalProposalHost::new(
                Arc::new(DesktopGoalProposalPolicy(config.clone())),
                resolver,
                bus,
            ),
            #[cfg(test)]
            config,
        }
    }

    pub(super) fn bind(&self, owner: &Arc<ConversationOrchestrator>) {
        self.inner.bind(owner);
    }
}

#[async_trait]
impl GoalProposalHost for DesktopGoalProposalHost {
    fn is_available(&self) -> bool {
        self.inner.is_available()
    }
    async fn state(&self) -> GoalProposalState {
        self.inner.state().await
    }
    async fn submit(&self, condition: Utf16JsonProjection, ask_user: bool) -> Result<(), String> {
        self.inner.submit(condition, ask_user).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const PENDING_MESSAGE: &str = "A goal proposal is already awaiting the user's decision. Keep working; if it is approved you will receive a kickoff message.";
    use lingxi_core::settings::{SettingsRawLayer, schema::SettingsJson};

    fn settings(layers: &[(Source, &str)]) -> EffectiveSettings {
        let mut settings = EffectiveSettings {
            settings: SettingsJson::default(),
            trace: Default::default(),
            effort_layers: Vec::new(),
            source_projection: None,
            source_layers: Vec::new(),
            typed_view_unrepresented_fields: Vec::new(),
            managed_typed_view_is_partial: false,
            source_incomplete: false,
            source_message: None,
            source_diagnostics: None,
            fusion_layers: Vec::new(),
        };
        settings.source_layers = layers
            .iter()
            .map(|(source, value)| SettingsRawLayer {
                source: *source,
                path: None,
                projection: Utf16JsonProjection::parse(&format!(
                    "{{\"modelProposedGoals\":\"{value}\"}}"
                ))
                .unwrap(),
            })
            .collect();
        settings
    }

    #[test]
    fn model_proposed_goals_uses_trusted_sources_and_native_precedence() {
        assert_eq!(
            trusted_setting(&settings(&[
                (Source::Project, "auto"),
                (Source::Local, "disabled")
            ])),
            ModelProposedGoals::Auto,
        );
        assert_eq!(
            trusted_setting(&settings(&[
                (Source::User, "auto"),
                (Source::Cli, "alwaysAsk"),
                (Source::Managed, "disabled"),
            ])),
            ModelProposedGoals::Disabled,
        );
        assert_eq!(
            trusted_setting(&settings(&[
                (Source::User, "disabled"),
                (Source::Cli, "auto")
            ])),
            ModelProposedGoals::Auto,
        );
    }

    #[test]
    fn incomplete_model_proposed_goals_state_never_grants_direct_consent() {
        let mut settings = settings(&[(Source::User, "auto")]);
        settings.source_incomplete = true;
        assert_eq!(trusted_setting(&settings), ModelProposedGoals::AlwaysAsk);
        settings.source_layers[0].projection =
            Utf16JsonProjection::parse(r#"{"modelProposedGoals":"disabled"}"#).unwrap();
        assert_eq!(trusted_setting(&settings), ModelProposedGoals::Disabled);
    }

    type Answer = Vec<(Utf16JsonProjection, Utf16JsonProjection)>;
    type Request = (Vec<Question>, tokio::sync::oneshot::Sender<Answer>);

    struct ControlledResolver(tokio::sync::mpsc::UnboundedSender<Request>);
    #[async_trait]
    impl AskUserQuestionResolver for ControlledResolver {
        async fn resolve(
            &self,
            questions: &[Question],
            _: bool,
        ) -> Result<Answer, tool_api::tool_trait::ToolError> {
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.0.send((questions.to_vec(), tx)).unwrap();
            rx.await.map_err(|_| {
                tool_api::tool_trait::ToolError::Internal("cancelled test dialog".into())
            })
        }
    }

    #[derive(Default)]
    struct Queue(Mutex<Vec<(String, Utf16JsonProjection, GoalOrigin)>>);
    #[async_trait]
    impl orchestrator::prompt::mid_turn_input::MidTurnInputSource for Queue {
        fn supports_goal_proposals(&self) -> bool {
            true
        }
        async fn enqueue_goal_proposal(
            &self,
            id: String,
            body: Utf16JsonProjection,
            origin: GoalOrigin,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<(), String> {
            self.0.lock().unwrap().push((id, body, origin));
            Ok(())
        }
        async fn take_mid_turn_input(&self) -> Option<Utf16JsonProjection> {
            None
        }
    }

    #[derive(Default)]
    struct LiveGate(std::sync::atomic::AtomicBool);
    #[async_trait]
    impl lingxi_core::host::PermissionGate for LiveGate {
        async fn check(
            &self,
            _: &str,
            _: &Utf16JsonProjection,
        ) -> lingxi_core::host::PermissionDecision {
            lingxi_core::host::PermissionDecision::Allow
        }
        fn permission_mode(&self) -> Option<String> {
            Some(
                if self.0.load(std::sync::atomic::Ordering::SeqCst) {
                    "plan"
                } else {
                    "default"
                }
                .into(),
            )
        }
    }

    struct Fixture {
        host: Arc<DesktopGoalProposalHost>,
        owner: Arc<ConversationOrchestrator>,
        queue: Arc<Queue>,
        requests: tokio::sync::mpsc::UnboundedReceiver<Request>,
        gate: Arc<LiveGate>,
        root: tempfile::TempDir,
    }

    async fn fixture() -> Fixture {
        use orchestrator::test_support::{
            MockApiClient, MockOutputStream, StaticMemoryProvider, noop_hook_executor,
        };
        let root = tempfile::tempdir().unwrap();
        let mut config = DesktopConfig::default();
        config.cwd = root.path().to_owned();
        config.lingxi_home = root.path().join("config");
        config.setting_source_scope = (true, false);
        config.composition = Some(super::super::DesktopSessionComposition::InteractiveCli);
        config.managed_policy_snapshot_cache.replace(
            super::super::settings_watch::read_managed_settings_snapshot(
                &root.path().join("managed"),
            )
            .await,
        );
        std::fs::create_dir_all(&config.lingxi_home).unwrap();
        let queue = Arc::new(Queue::default());
        let gate = Arc::new(LiveGate::default());
        let owner = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new(
                orchestrator::OrchestratorConfig {
                    interactive_session: true,
                    ..Default::default()
                },
                Arc::new(MockApiClient::new(vec![])),
                Arc::new(tool_api::ToolRegistry::new()),
                noop_hook_executor(),
                gate.clone(),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                root.path().to_owned(),
            )
            .with_mid_turn_input(queue.clone()),
        );
        let (tx, requests) = tokio::sync::mpsc::unbounded_channel();
        let host = Arc::new(DesktopGoalProposalHost::new(
            config,
            Some(Arc::new(ControlledResolver(tx))),
            Arc::new(telemetry::AnalyticsBus::new()),
        ));
        host.bind(&owner);
        Fixture {
            host,
            owner,
            queue,
            requests,
            gate,
            root,
        }
    }

    async fn wait_for_pending_to_settle(host: &DesktopGoalProposalHost) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while host.inner.has_pending_proposal() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn proposal_host_returns_while_approval_is_pending_and_only_queues_after_approval() {
        let mut f = fixture().await;
        f.owner.set_active_goal(&"old goal".into()).await;
        f.host.submit("new goal".into(), true).await.unwrap();
        assert!(f.queue.0.lock().unwrap().is_empty());
        assert_eq!(
            f.owner
                .get_active_goal()
                .await
                .unwrap()
                .condition
                .string_at(""),
            Some("old goal")
        );
        assert_eq!(
            f.host.submit("second".into(), false).await.unwrap_err(),
            PENDING_MESSAGE
        );
        let (questions, answer) = f.requests.recv().await.unwrap();
        assert_eq!(questions[0].question.string_at(""), Some("new goal"));
        answer
            .send(vec![(questions[0].question.clone(), "Approve".into())])
            .unwrap();
        wait_for_pending_to_settle(&f.host).await;
        let queued = f.queue.0.lock().unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].1.string_at(""), Some("/goal new goal"));
        assert_eq!(queued[0].2, GoalOrigin::ProposalApproved);
        assert_eq!(
            f.owner
                .get_active_goal()
                .await
                .unwrap()
                .condition
                .string_at(""),
            Some("old goal")
        );
    }

    #[tokio::test]
    async fn proposal_host_direct_path_queues_without_opening_a_dialog() {
        let mut f = fixture().await;
        f.host.submit("direct goal".into(), false).await.unwrap();
        assert!(f.requests.try_recv().is_err());
        assert_eq!(f.queue.0.lock().unwrap()[0].2, GoalOrigin::ProposalDirect);
        assert!(f.owner.get_active_goal().await.is_none());
    }

    #[tokio::test]
    async fn proposal_host_decline_has_no_kickoff_or_goal_mutation() {
        let mut f = fixture().await;
        f.host.submit("declined goal".into(), true).await.unwrap();
        let (questions, answer) = f.requests.recv().await.unwrap();
        answer
            .send(vec![(questions[0].question.clone(), "Decline".into())])
            .unwrap();
        wait_for_pending_to_settle(&f.host).await;
        assert!(f.queue.0.lock().unwrap().is_empty());
        assert!(f.owner.get_active_goal().await.is_none());
    }

    #[tokio::test]
    async fn proposal_host_rechecks_plan_mode_and_disabled_setting_after_approval() {
        for plan in [false, true] {
            let mut f = fixture().await;
            f.host.submit("goal".into(), true).await.unwrap();
            let (questions, answer) = f.requests.recv().await.unwrap();
            if plan {
                f.gate.0.store(true, std::sync::atomic::Ordering::SeqCst);
            } else {
                std::fs::write(
                    f.host.config.lingxi_home.join("settings.json"),
                    r#"{"modelProposedGoals":"disabled"}"#,
                )
                .unwrap();
                assert_eq!(f.host.state().await.setting, ModelProposedGoals::Disabled);
            }
            answer
                .send(vec![(questions[0].question.clone(), "Approve".into())])
                .unwrap();
            wait_for_pending_to_settle(&f.host).await;
            assert!(f.queue.0.lock().unwrap().is_empty());
            assert!(f.owner.get_active_goal().await.is_none());
        }
    }

    #[tokio::test]
    async fn proposal_host_new_session_can_submit_before_old_dialog_cleanup() {
        let mut f = fixture().await;
        f.host.submit("old session".into(), true).await.unwrap();
        let (_, old_answer) = f.requests.recv().await.unwrap();
        f.owner.clear_session().await.unwrap();
        f.host.submit("new session".into(), true).await.unwrap();
        let (questions, answer) = f.requests.recv().await.unwrap();
        let _ = old_answer.send(vec![("old session".into(), "Approve".into())]);
        answer
            .send(vec![(questions[0].question.clone(), "Approve".into())])
            .unwrap();
        wait_for_pending_to_settle(&f.host).await;
        let queued = f.queue.0.lock().unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].1.string_at(""), Some("/goal new session"));
        assert!(f.root.path().exists());
    }
}
