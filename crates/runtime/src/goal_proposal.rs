//! Nonblocking Goal proposal approval on the owning interactive session.

use async_trait::async_trait;
use lingxi_core::host::OrchestratorHandle;
use lingxi_core::host::goal::{GoalProposalHost, GoalProposalState, ModelProposedGoals};
use lingxi_core::session::GoalOrigin;
use lingxi_core::settings::{EffectiveSettings, tracer::Source};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use orchestrator::ConversationOrchestrator;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tool_ui::ask_user_question::{AskUserQuestionResolver, Question, QuestionOption};

const PENDING_MESSAGE: &str = "A goal proposal is already awaiting the user's decision. Keep working; if it is approved you will receive a kickoff message.";
const NO_DIALOG_MESSAGE: &str = "Goal proposals need an interactive session to render the approval prompt; none is available here.";
const HOOKS_MESSAGE: &str = "/goal can't run while hooks are restricted (disableAllHooks or allowManagedHooksOnly is set in settings or by policy).";
const HELD_MESSAGE: &str = "/goal can't run yet: your organization's settings haven't been read, so it isn't known whether it allows hooks.";
const TRUST_MESSAGE: &str = "/goal is only available in trusted workspaces. Restart, accept the trust dialog, and try again.";

/// Trusted sources have Native's policy, flag, user precedence. Repository
/// settings are deliberately excluded from this consent decision.
pub(crate) fn trusted_setting(settings: &EffectiveSettings) -> ModelProposedGoals {
    let mut selected = None;
    for source in [Source::Managed, Source::Cli, Source::User] {
        for layer in settings.source_layers.iter().rev() {
            if layer.source != source {
                continue;
            }
            selected = match layer.projection.string_at("/modelProposedGoals") {
                Some("disabled") => Some(ModelProposedGoals::Disabled),
                Some("alwaysAsk") => Some(ModelProposedGoals::AlwaysAsk),
                Some("auto") => Some(ModelProposedGoals::Auto),
                _ => None,
            };
            if selected.is_some() {
                break;
            }
        }
        if selected.is_some() {
            break;
        }
    }
    if settings.source_incomplete && selected != Some(ModelProposedGoals::Disabled) {
        ModelProposedGoals::AlwaysAsk
    } else {
        selected.unwrap_or_default()
    }
}

/// Host-owned launch facts and trusted consent settings for one local session.
pub(crate) trait GoalProposalPolicy: Send + Sync {
    fn is_interactive_local(&self) -> bool;
    fn setting(&self, owner: &ConversationOrchestrator) -> ModelProposedGoals;
}

pub(crate) struct SessionGoalProposalHost {
    owner: OnceLock<Weak<ConversationOrchestrator>>,
    policy: Arc<dyn GoalProposalPolicy>,
    resolver: Option<Arc<dyn AskUserQuestionResolver>>,
    pending: Arc<Mutex<Option<(uuid::Uuid, u64)>>>,
    bus: Arc<telemetry::AnalyticsBus>,
}

impl SessionGoalProposalHost {
    pub(crate) fn new(
        policy: Arc<dyn GoalProposalPolicy>,
        resolver: Option<Arc<dyn AskUserQuestionResolver>>,
        bus: Arc<telemetry::AnalyticsBus>,
    ) -> Self {
        Self {
            owner: OnceLock::new(),
            policy,
            resolver,
            pending: Arc::new(Mutex::new(None)),
            bus,
        }
    }

    pub(crate) fn bind(&self, owner: &Arc<ConversationOrchestrator>) {
        let _ = self.owner.set(Arc::downgrade(owner));
    }

    #[cfg(test)]
    pub(crate) fn has_pending_proposal(&self) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    fn owner(&self) -> Option<Arc<ConversationOrchestrator>> {
        self.owner.get().and_then(Weak::upgrade)
    }

    async fn live_state(&self, owner: &ConversationOrchestrator) -> GoalProposalState {
        let evaluator_gate_error = if owner.hooks_restricted().await {
            Some(HOOKS_MESSAGE.to_owned())
        } else if owner.goal_hooks_held().await {
            Some(HELD_MESSAGE.to_owned())
        } else if !owner.workspace_trusted().await {
            Some(TRUST_MESSAGE.to_owned())
        } else {
            None
        };
        GoalProposalState {
            interactive_local: self.policy.is_interactive_local(),
            evaluator_gate_error,
            plan_mode: owner.permission_mode().as_deref() == Some("plan"),
            setting: self.policy.setting(owner),
            dialog_available: self.resolver.is_some() && owner.goal_proposal_queue_available(),
        }
    }
}

#[async_trait]
impl GoalProposalHost for SessionGoalProposalHost {
    fn is_available(&self) -> bool {
        if !self.policy.is_interactive_local() || self.resolver.is_none() {
            return false;
        }
        let Some(owner) = self.owner() else {
            return false;
        };
        owner.goal_proposal_queue_available()
            && self.policy.setting(&owner) != ModelProposedGoals::Disabled
    }

    async fn state(&self) -> GoalProposalState {
        match self.owner() {
            Some(owner) => self.live_state(&owner).await,
            None => GoalProposalState::default(),
        }
    }

    async fn submit(&self, condition: Utf16JsonProjection, ask_user: bool) -> Result<(), String> {
        let owner = self.owner().ok_or(NO_DIALOG_MESSAGE)?;
        let resolver = self.resolver.clone().ok_or(NO_DIALOG_MESSAGE)?;
        if !owner.goal_proposal_queue_available() {
            return Err(NO_DIALOG_MESSAGE.into());
        }
        let (epoch, cancel) = owner.goal_proposal_scope();
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending
                .as_ref()
                .is_some_and(|(_, pending_epoch)| *pending_epoch != epoch)
            {
                *pending = None;
            }
            if pending.is_some() {
                return Err(PENDING_MESSAGE.into());
            }
        }
        if !ask_user {
            return owner
                .queue_goal_proposal_kickoff(condition, GoalOrigin::ProposalDirect, epoch)
                .await;
        }
        let id = uuid::Uuid::new_v4();
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if pending.is_some() {
                return Err(PENDING_MESSAGE.into());
            }
            *pending = Some((id, epoch));
        }
        let weak_owner = Arc::downgrade(&owner);
        let policy = self.policy.clone();
        let pending = self.pending.clone();
        let bus = self.bus.clone();
        tokio::spawn(async move {
            let question = Question {
                question: condition.clone(),
                header: "Set goal?".into(),
                multi_select: false,
                options: vec![
                    QuestionOption {
                        label: "Approve".into(),
                        description: "Set this session goal and keep working until it is met."
                            .into(),
                        preview: None,
                    },
                    QuestionOption {
                        label: "Decline".into(),
                        description: "Continue working without setting this goal.".into(),
                        preview: None,
                    },
                ],
            };
            let questions = [question];
            let answer = tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                answer = resolver.resolve(&questions, false) => answer.ok(),
            };
            let approved = answer.as_ref().is_some_and(|answers| {
                answers.iter().any(|(question, answer)| {
                    question == &condition && answer.string_at("") == Some("Approve")
                })
            });
            let explicit_decline = answer.as_ref().is_some_and(|answers| {
                answers.iter().any(|(question, answer)| {
                    question == &condition && answer.string_at("") == Some("Decline")
                })
            });
            let mut decision = if approved {
                "approved_stale"
            } else if explicit_decline {
                "declined"
            } else {
                "unanswered"
            };
            if approved && !cancel.is_cancelled() {
                if let Some(owner) = weak_owner.upgrade() {
                    let current = owner.goal_proposal_scope();
                    let is_current = current.0 == epoch
                        && !current.1.is_cancelled()
                        && *pending
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            == Some((id, epoch));
                    if is_current {
                        let setting = policy.setting(&owner);
                        if setting == ModelProposedGoals::Disabled {
                            decision = "approved_disabled";
                        } else if owner.permission_mode().as_deref() == Some("plan") {
                            decision = "approved_plan_mode";
                        } else {
                            decision = "approved";
                            if let Err(error) = owner
                                .queue_goal_proposal_kickoff(
                                    condition,
                                    GoalOrigin::ProposalApproved,
                                    epoch,
                                )
                                .await
                            {
                                tracing::debug!(%error, "goal proposal kickoff was retired");
                            }
                        }
                    }
                }
            }
            let mut metadata = telemetry::LogEventMetadata::new();
            metadata.insert(
                "decision".into(),
                telemetry::AnalyticsValue::String(decision.into()),
            );
            bus.log_event("tengu_goal_proposal_decided", metadata).await;
            let mut pending = pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *pending == Some((id, epoch)) {
                *pending = None;
            }
        });
        Ok(())
    }
}
