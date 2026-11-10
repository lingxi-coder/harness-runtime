//! Session ownership for nonblocking Goal proposals and their queued kickoff.

use super::ConversationOrchestrator;
use lingxi_core::session::GoalOrigin;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::collections::HashSet;
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub(crate) struct GoalProposalRuntime {
    state: Mutex<ProposalGeneration>,
}

struct ProposalGeneration {
    epoch: u64,
    closed: bool,
    cancel: CancellationToken,
    queued_origin: Option<(Utf16JsonProjection, GoalOrigin, String)>,
    queued_ids: HashSet<String>,
}

impl Default for GoalProposalRuntime {
    fn default() -> Self {
        Self {
            state: Mutex::new(ProposalGeneration {
                epoch: 0,
                closed: false,
                cancel: CancellationToken::new(),
                queued_origin: None,
                queued_ids: HashSet::new(),
            }),
        }
    }
}

impl Drop for GoalProposalRuntime {
    fn drop(&mut self) {
        self.state
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancel
            .cancel();
    }
}

impl ConversationOrchestrator {
    /// Whether the owning host can enqueue a proposal through its ordinary
    /// between-turn slash-command lifecycle.
    #[must_use]
    pub fn goal_proposal_queue_available(&self) -> bool {
        !self
            .lifecycle_runtime
            .goal_proposal
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed
            && self
                .mid_turn_input
                .get()
                .is_some_and(|source| source.supports_goal_proposals())
    }

    /// Capture the mounted session generation without retaining the session.
    #[must_use]
    pub fn goal_proposal_scope(&self) -> (u64, CancellationToken) {
        let state = self
            .lifecycle_runtime
            .goal_proposal
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.epoch, state.cancel.clone())
    }

    pub(crate) fn invalidate_goal_proposals(&self) {
        let mut state = self
            .lifecycle_runtime
            .goal_proposal
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cancel.cancel();
        state.epoch = state.epoch.wrapping_add(1);
        state.cancel = CancellationToken::new();
        if state.closed {
            state.cancel.cancel();
        }
        state.queued_origin = None;
        state.queued_ids.clear();
    }

    /// Permanently retire proposal approval and queue admission before host
    /// teardown starts. Retained handles and later session resets cannot reopen it.
    pub fn close_goal_proposal_admission(&self) {
        let mut state = self
            .lifecycle_runtime
            .goal_proposal
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.closed {
            state.closed = true;
            state.epoch = state.epoch.wrapping_add(1);
        }
        state.cancel.cancel();
        state.queued_origin = None;
        state.queued_ids.clear();
    }

    /// Claim an internally queued proposal exactly once in the host's owned
    /// slash-dispatch slot. Retired session generations cannot claim a kickoff.
    #[must_use]
    pub fn claim_queued_goal_proposal(&self, id: &str) -> bool {
        let mut state = self
            .lifecycle_runtime
            .goal_proposal
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        !state.closed && !state.cancel.is_cancelled() && state.queued_ids.remove(id)
    }

    pub(crate) fn take_queued_goal_origin(&self, condition: &Utf16JsonProjection) -> GoalOrigin {
        let mut state = self
            .lifecycle_runtime
            .goal_proposal
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state
            .queued_origin
            .as_ref()
            .is_some_and(|(queued, _, _)| queued == condition)
        {
            state
                .queued_origin
                .take()
                .expect("matching queued origin exists")
                .1
        } else {
            GoalOrigin::User
        }
    }

    /// Queue the ordinary `/goal` command. This does not mutate the active goal;
    /// the normal command dispatcher owns activation, hooks and persistence.
    ///
    /// # Errors
    /// Fails when the session has no real queue, or its generation was retired.
    pub async fn queue_goal_proposal_kickoff(
        &self,
        condition: Utf16JsonProjection,
        origin: GoalOrigin,
        epoch: u64,
    ) -> Result<(), String> {
        let source = self
            .mid_turn_input
            .get()
            .filter(|source| source.supports_goal_proposals())
            .ok_or_else(|| "Goal proposals need an interactive session to render the approval prompt; none is available here.".to_owned())?;
        let id = format!("goal-proposal:{}", uuid::Uuid::new_v4());
        let cancel = {
            let mut state = self
                .lifecycle_runtime
                .goal_proposal
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.closed || state.epoch != epoch || state.cancel.is_cancelled() {
                return Err("The goal proposal belongs to a previous session.".into());
            }
            state.queued_origin = Some((condition.clone(), origin, id.clone()));
            state.queued_ids.insert(id.clone());
            state.cancel.clone()
        };
        let mut units = "/goal ".encode_utf16().collect::<Vec<_>>();
        units.extend(condition.string_units("").unwrap_or_default());
        let body = Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
            .expect("goal command is a source string");
        let result = source
            .enqueue_goal_proposal(id.clone(), body, origin, cancel)
            .await;
        if result.is_err() {
            let mut state = self
                .lifecycle_runtime
                .goal_proposal
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.queued_ids.remove(&id);
            if state
                .queued_origin
                .as_ref()
                .is_some_and(|(_, _, queued)| queued == &id)
            {
                state.queued_origin = None;
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
        noop_hook_executor,
    };
    use lingxi_core::host::OrchestratorHandle;
    use std::sync::Arc;

    #[derive(Default)]
    struct Queue(Mutex<Vec<(String, Utf16JsonProjection, GoalOrigin, CancellationToken)>>);

    #[async_trait::async_trait]
    impl crate::prompt::mid_turn_input::MidTurnInputSource for Queue {
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
            self.0.lock().unwrap().push((id, body, origin, cancel));
            Ok(())
        }
        async fn take_mid_turn_input(&self) -> Option<Utf16JsonProjection> {
            None
        }
    }

    fn fixture() -> (Arc<ConversationOrchestrator>, Arc<Queue>) {
        let queue = Arc::new(Queue::default());
        let owner = ConversationOrchestrator::new(
            crate::OrchestratorConfig {
                interactive_session: true,
                ..Default::default()
            },
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(tool_api::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_mid_turn_input(queue.clone());
        (ConversationOrchestrator::into_shared(owner), queue)
    }

    #[tokio::test]
    async fn proposal_kickoff_keeps_existing_goal_until_claimed_command_runs() {
        let (owner, queue) = fixture();
        owner.set_active_goal(&"old goal".into()).await;
        let (epoch, _) = owner.goal_proposal_scope();
        owner
            .queue_goal_proposal_kickoff("new goal".into(), GoalOrigin::ProposalDirect, epoch)
            .await
            .unwrap();
        assert_eq!(
            owner
                .get_active_goal()
                .await
                .unwrap()
                .condition
                .string_at(""),
            Some("old goal")
        );
        let queued = queue.0.lock().unwrap();
        assert_eq!(queued[0].1.string_at(""), Some("/goal new goal"));
        assert_eq!(queued[0].2, GoalOrigin::ProposalDirect);
        let id = queued[0].0.clone();
        drop(queued);
        assert!(owner.claim_goal_proposal(&id).await);
        assert!(!owner.claim_goal_proposal(&id).await);
        owner.set_active_goal(&"new goal".into()).await;
        assert_eq!(
            owner
                .session
                .lock()
                .await
                .active_goal
                .as_ref()
                .unwrap()
                .origin,
            GoalOrigin::ProposalDirect
        );
        owner.set_active_goal(&"new goal".into()).await;
        assert_eq!(
            owner
                .session
                .lock()
                .await
                .active_goal
                .as_ref()
                .unwrap()
                .origin,
            GoalOrigin::User,
            "the staged origin is consumed once for the matching condition"
        );
    }

    #[tokio::test]
    async fn session_retirement_invalidates_dequeued_proposal_identity_and_timer() {
        let (owner, queue) = fixture();
        let (epoch, token) = owner.goal_proposal_scope();
        owner
            .queue_goal_proposal_kickoff("goal".into(), GoalOrigin::ProposalApproved, epoch)
            .await
            .unwrap();
        let id = queue.0.lock().unwrap()[0].0.clone();
        owner.invalidate_goal_proposals();
        assert!(token.is_cancelled());
        assert!(!owner.claim_goal_proposal(&id).await);
        assert_eq!(
            owner.take_queued_goal_origin(&"goal".into()),
            GoalOrigin::User
        );
        assert!(
            owner
                .queue_goal_proposal_kickoff("stale".into(), GoalOrigin::ProposalApproved, epoch)
                .await
                .is_err()
        );
        assert_eq!(queue.0.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rejected_goal_set_consumes_only_the_matching_proposal_origin() {
        let (owner, _) = fixture();
        let (epoch, _) = owner.goal_proposal_scope();
        owner
            .queue_goal_proposal_kickoff(
                "approved condition".into(),
                GoalOrigin::ProposalApproved,
                epoch,
            )
            .await
            .unwrap();
        owner.retire_goal_set_origin(&"unrelated".into()).await;
        owner
            .retire_goal_set_origin(&"approved condition".into())
            .await;
        owner.set_active_goal(&"approved condition".into()).await;
        assert_eq!(
            owner
                .session
                .lock()
                .await
                .active_goal
                .as_ref()
                .unwrap()
                .origin,
            GoalOrigin::User
        );
    }

    #[tokio::test]
    async fn ordinary_goal_clear_keeps_pending_proposal_valid() {
        let (owner, queue) = fixture();
        owner.set_active_goal(&"old".into()).await;
        let (epoch, token) = owner.goal_proposal_scope();
        owner
            .queue_goal_proposal_kickoff("new".into(), GoalOrigin::ProposalApproved, epoch)
            .await
            .unwrap();
        owner.clear_active_goal().await;
        assert!(!token.is_cancelled());
        let id = queue.0.lock().unwrap()[0].0.clone();
        assert!(owner.claim_goal_proposal(&id).await);
        owner.set_active_goal(&"new".into()).await;
        assert_eq!(
            owner
                .session
                .lock()
                .await
                .active_goal
                .as_ref()
                .unwrap()
                .origin,
            GoalOrigin::ProposalApproved
        );
    }

    #[tokio::test]
    async fn host_teardown_permanently_closes_proposals_even_after_a_session_reset() {
        let (owner, queue) = fixture();
        let (epoch, cancel) = owner.goal_proposal_scope();
        owner
            .queue_goal_proposal_kickoff(
                "before teardown".into(),
                GoalOrigin::ProposalDirect,
                epoch,
            )
            .await
            .unwrap();
        let id = queue.0.lock().unwrap()[0].0.clone();
        owner.close_background_producer_admission();
        assert!(cancel.is_cancelled());
        assert!(!owner.goal_proposal_queue_available());
        assert!(!owner.claim_goal_proposal(&id).await);
        owner.clear_session().await.unwrap();
        let (new_epoch, new_cancel) = owner.goal_proposal_scope();
        assert!(new_cancel.is_cancelled());
        assert!(!owner.goal_proposal_queue_available());
        assert!(
            owner
                .queue_goal_proposal_kickoff(
                    "after teardown".into(),
                    GoalOrigin::ProposalApproved,
                    new_epoch
                )
                .await
                .is_err()
        );
        assert_eq!(queue.0.lock().unwrap().len(), 1);
    }
}
