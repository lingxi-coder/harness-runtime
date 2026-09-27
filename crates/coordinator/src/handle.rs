//! `MailboxRouterHandle` trait impl for `MailboxRouter`.
//!
//! Bridges the `lingxi-traits` seam (string-based agent ids + claim-window
//! ack) to the concrete `MailboxRouter::route` (UUID-based `AgentId`) so
//! `SendMessageTool` in `lingxi-tools` can dispatch without taking a cyclic
//! dep on `lingxi-coordinator`.
//!
//! The 30-second `claim_window_secs` is byte-locked to
//! `SEND_MESSAGE_CLAIM_WINDOW` in `lingxi-tool_api::builtin::send_message`.

use crate::mailbox::{MailboxRouter, MessageSender, TeammateMessage};
use crate::team_registry::{TeamRegistry, WorkerStatus};
use async_trait::async_trait;
use platform_api::mailbox::{MailboxError, MailboxMessage, MailboxRouterHandle, RouteAck};
use platform_api::team_registry::{TeamRegistryHandle, WorkerInfo};
use protocol::AgentId;
use std::time::SystemTime;
use uuid::Uuid;

/// Byte-locked teammate claim window (spec §7 line 498).
pub const CLAIM_WINDOW_SECS: u64 = 30;

/// The lead member's name (TS `TEAM_LEAD_NAME = "team-lead"`,
/// `utils/swarm/constants.ts:1`). The `TaskUpdate` owner-change notification
/// uses this literal as the sender when no teammate name is bound (the leader /
/// main thread), so `route()` must ACCEPT it rather than require an id.
const TEAM_LEAD_NAME: &str = "team-lead";

/// Parse a bare UUID recipient into an [`AgentId`].
fn try_parse_agent_id(s: &str) -> Option<AgentId> {
    Uuid::parse_str(s).ok().map(AgentId::from_uuid)
}

#[async_trait]
impl MailboxRouterHandle for MailboxRouter {
    async fn teammate_color(&self, name: &str) -> Option<String> {
        MailboxRouter::teammate_color(self, name).await
    }

    async fn route(
        &self,
        from_agent: &str,
        to_agent: &str,
        message: MailboxMessage,
    ) -> Result<RouteAck, MailboxError> {
        // Resolve the SENDER label into a `MessageSender`. claude-code addresses
        // mailboxes by NAME; the common `TaskUpdate` notification sender is the
        // literal `"team-lead"` (→ the coordinator) when no teammate name is
        // bound. A bare uuid / `agent:<uuid>` → `Teammate(id)`; a registered
        // display name → `Teammate(id)`; anything else (incl. `team-lead`) →
        // `Coordinator`, so a stray sender label never drops the message
        // (parity with TS, where `from` is just a label).
        let from = if from_agent.eq_ignore_ascii_case(TEAM_LEAD_NAME) {
            MessageSender::Coordinator
        } else if let Some(id) =
            try_parse_agent_id(from_agent.strip_prefix("agent:").unwrap_or(from_agent))
        {
            MessageSender::Teammate(id)
        } else if let Some(id) = self.resolve_name(from_agent).await {
            MessageSender::Teammate(id)
        } else {
            MessageSender::Coordinator
        };

        // Resolve the RECIPIENT into an [`AgentId`]: a bare uuid
        // first, then a registered display NAME (the common case — `TaskUpdate`
        // assigns ownership by name), then any additional registered address.
        // The last one is what makes a background agent's TASK id routable: a
        // completion `<task-notification>` carries that id, and the coordinator
        // prompt tells the model to continue the agent by sending to it.
        // An unresolvable recipient is `NotFound`, so a genuinely unknown
        // teammate still surfaces cleanly.
        let to_id = if let Some(id) = try_parse_agent_id(to_agent) {
            id
        } else if let Some(id) = self.resolve_name(to_agent).await {
            id
        } else if let Some(id) = self.resolve_alias(to_agent).await {
            id
        } else {
            return Err(MailboxError::NotFound(format!(
                "unknown recipient '{to_agent}'"
            )));
        };

        let teammate_msg = TeammateMessage {
            from,
            from_name: from_agent.to_string(),
            content: message.content,
            summary: None,
            message_id: message.message_id,
            timestamp: message.timestamp,
            request_id: None,
        };

        // Adapt the concrete error space onto the trait's error type.
        match self.route(&to_id, teammate_msg).await {
            Ok(()) => Ok(RouteAck {
                claimed_at: SystemTime::now(),
                claim_window_secs: CLAIM_WINDOW_SECS,
            }),
            Err(crate::mailbox::MailboxError::NotFound(id)) => {
                Err(MailboxError::NotFound(id.to_string()))
            }
            Err(crate::mailbox::MailboxError::Full) => Err(MailboxError::Full),
            Err(crate::mailbox::MailboxError::Closed) => Err(MailboxError::Closed),
        }
    }

    async fn broadcast(
        &self,
        from_agent: &str,
        message: MailboxMessage,
    ) -> Result<Vec<String>, MailboxError> {
        let sender_id = if from_agent.eq_ignore_ascii_case(TEAM_LEAD_NAME) {
            None
        } else if let Some(id) =
            try_parse_agent_id(from_agent.strip_prefix("agent:").unwrap_or(from_agent))
        {
            Some(id)
        } else {
            self.resolve_name(from_agent).await
        };
        let recipients = self.named_recipients().await;
        let mut delivered = Vec::with_capacity(recipients.len());
        for (name, agent_id) in recipients {
            if Some(agent_id) == sender_id {
                continue;
            }
            let teammate_msg = TeammateMessage {
                from: sender_id.map_or(MessageSender::Coordinator, MessageSender::Teammate),
                from_name: from_agent.to_string(),
                content: message.content.clone(),
                summary: None,
                message_id: message.message_id.clone(),
                timestamp: message.timestamp,
                request_id: None,
            };
            if self.route(&agent_id, teammate_msg).await.is_ok() {
                delivered.push(name);
            }
        }
        delivered.sort();
        Ok(delivered)
    }

    async fn named_recipients(&self) -> Vec<(String, AgentId)> {
        MailboxRouter::named_recipients(self).await
    }
}

/// Canonical lowering of [`WorkerStatus`] to the simplified `WorkerInfo.status`
/// label string consumed by the bridge roster DTO (T18) and the TUI
/// `WorkerRow` (T20). Defined here so the read seam and the (future) lowering
/// share one source of truth.
#[must_use]
pub fn worker_status_label(status: &WorkerStatus) -> String {
    match status {
        WorkerStatus::Idle => "idle",
        WorkerStatus::Working { .. } => "working",
        WorkerStatus::AwaitingMessage => "awaiting_message",
        WorkerStatus::Completed => "completed",
        WorkerStatus::Failed { .. } => "failed",
        WorkerStatus::Killed => "killed",
    }
    .to_string()
}

pub(crate) fn worker_info(worker: crate::team_registry::WorkerAgent) -> WorkerInfo {
    WorkerInfo {
        awaiting_plan_approval: worker.awaiting_plan_approval,
        agent_id: worker.agent_id.to_string(),
        agent_type: worker.agent_type,
        name: worker.name,
        status: worker_status_label(&worker.status),
    }
}

#[async_trait]
impl TeamRegistryHandle for TeamRegistry {
    async fn list_workers(&self) -> Vec<WorkerInfo> {
        self.list().await.into_iter().map(worker_info).collect()
    }

    async fn team_name(&self) -> Option<String> {
        TeamRegistry::team_name(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mailbox::TeammateMailbox;
    use std::sync::Arc;

    #[test]
    fn recipient_parser_rejects_internal_display_prefix() {
        let id = AgentId::new();
        assert_eq!(try_parse_agent_id(&id.as_uuid().to_string()), Some(id));
        assert_eq!(try_parse_agent_id(&id.to_string()), None);
    }

    #[test]
    fn claim_window_locked_30s() {
        assert_eq!(CLAIM_WINDOW_SECS, 30);
    }

    #[tokio::test]
    async fn route_via_handle_returns_30s_claim_window() {
        let router = MailboxRouter::new();
        let to = AgentId::new();
        let from = AgentId::new();
        router
            .register(to, Arc::new(TeammateMailbox::new(to)))
            .await;

        let h: &dyn MailboxRouterHandle = &router;
        let ack = h
            .route(
                &from.as_uuid().to_string(),
                &to.as_uuid().to_string(),
                MailboxMessage {
                    message_id: "m1".into(),
                    content: "hi".into(),
                    timestamp: SystemTime::now(),
                    color: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(ack.claim_window_secs, 30);
    }

    #[tokio::test]
    async fn route_accepts_a_registered_alias_as_the_recipient() {
        // The gap this closes: a completion notification hands the model the
        // background agent's task id, the prompt tells it to continue that agent
        // by sending to the id, and the send came back "unknown recipient"
        // because only uuids and display names resolved.
        let router = MailboxRouter::new();
        let to = AgentId::new();
        router
            .register(to, Arc::new(TeammateMailbox::new(to)))
            .await;
        router.register_alias("a1b2c3d4e", to).await;

        let h: &dyn MailboxRouterHandle = &router;
        h.route(
            "team-lead",
            "a1b2c3d4e",
            MailboxMessage {
                message_id: "m1".into(),
                content: "carry on".into(),
                timestamp: SystemTime::now(),
                color: None,
            },
        )
        .await
        .expect("a registered alias must resolve");

        // An unregistered string is still a clean NotFound.
        let err = h
            .route(
                "team-lead",
                "not-an-agent",
                MailboxMessage {
                    message_id: "m2".into(),
                    content: "x".into(),
                    timestamp: SystemTime::now(),
                    color: None,
                },
            )
            .await
            .expect_err("an unknown recipient must not resolve");
        assert!(matches!(err, MailboxError::NotFound(_)));
    }

    #[tokio::test]
    async fn route_via_handle_unknown_recipient_is_not_found() {
        let router = MailboxRouter::new();
        let h: &dyn MailboxRouterHandle = &router;
        let from = AgentId::new();
        let to = AgentId::new();
        let err = h
            .route(
                &from.as_uuid().to_string(),
                &to.as_uuid().to_string(),
                MailboxMessage {
                    message_id: "m1".into(),
                    content: "hi".into(),
                    timestamp: SystemTime::now(),
                    color: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, MailboxError::NotFound(_)));
    }

    /// T7: a `team-lead` → named-teammate task-assignment notification delivers.
    /// The sender is the literal `"team-lead"` (not a uuid) and the recipient is
    /// addressed by NAME — exactly the shape `TaskUpdate` produces when the
    /// leader claims a task for a teammate. Previously the id-only router dropped
    /// both, so the message never reached the teammate.
    #[tokio::test]
    async fn route_team_lead_to_named_teammate_delivers() {
        let reg = TeamRegistry::new(AgentId::new());
        let teammate = reg
            .spawn_worker("explorer".into(), "Researcher".into(), String::new())
            .await
            .unwrap();

        // Route through the trait surface the tool actually uses.
        let h: &dyn MailboxRouterHandle = reg.mailbox_router.as_ref();
        let ack = h
            .route(
                "team-lead",
                "researcher", // case-insensitive name resolution
                MailboxMessage {
                    message_id: "m1".into(),
                    content: r#"{"type":"task_assignment"}"#.into(),
                    timestamp: SystemTime::now(),
                    color: None,
                },
            )
            .await
            .expect("team-lead → named teammate must deliver");
        assert_eq!(ack.claim_window_secs, 30);

        // The message landed in the named teammate's mailbox, sender = Coordinator.
        let mailbox = reg
            .mailbox_router
            .get(&teammate)
            .await
            .expect("teammate mailbox registered");
        let drained = mailbox.drain();
        assert_eq!(drained.len(), 1, "exactly one message delivered");
        assert!(matches!(drained[0].from, MessageSender::Coordinator));
        assert_eq!(drained[0].content, r#"{"type":"task_assignment"}"#);
    }

    /// A teammate-name SENDER resolves to `Teammate(id)`, and an unknown
    /// recipient NAME is `NotFound` (not silently dropped).
    #[tokio::test]
    async fn route_resolves_sender_name_and_rejects_unknown_recipient_name() {
        let reg = TeamRegistry::new(AgentId::new());
        let sender = reg
            .spawn_worker("writer".into(), "scribe".into(), String::new())
            .await
            .unwrap();
        let recipient = reg
            .spawn_worker("explorer".into(), "scout".into(), String::new())
            .await
            .unwrap();

        let h: &dyn MailboxRouterHandle = reg.mailbox_router.as_ref();
        h.route(
            "scribe",
            "scout",
            MailboxMessage {
                message_id: "m2".into(),
                content: "hi".into(),
                timestamp: SystemTime::now(),
                color: None,
            },
        )
        .await
        .expect("named sender → named recipient delivers");

        let drained = reg.mailbox_router.get(&recipient).await.unwrap().drain();
        assert_eq!(drained.len(), 1);
        assert!(
            matches!(drained[0].from, MessageSender::Teammate(id) if id == sender),
            "named sender resolves to Teammate(id)"
        );

        // Unknown recipient NAME → NotFound.
        let err = h
            .route(
                "team-lead",
                "ghost",
                MailboxMessage {
                    message_id: "m3".into(),
                    content: "x".into(),
                    timestamp: SystemTime::now(),
                    color: None,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, MailboxError::NotFound(_)));
    }

    #[tokio::test]
    async fn broadcast_delivers_to_every_named_teammate_except_sender() {
        let reg = TeamRegistry::new(AgentId::new());
        let alpha = reg
            .spawn_worker("explorer".into(), "alpha".into(), String::new())
            .await
            .unwrap();
        let beta = reg
            .spawn_worker("writer".into(), "beta".into(), String::new())
            .await
            .unwrap();
        let gamma = reg
            .spawn_worker("reviewer".into(), "gamma".into(), String::new())
            .await
            .unwrap();

        let h: &dyn MailboxRouterHandle = reg.mailbox_router.as_ref();
        let delivered = h
            .broadcast(
                "alpha",
                MailboxMessage {
                    message_id: "broadcast-1".into(),
                    content: "status update".into(),
                    timestamp: SystemTime::now(),
                    color: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(delivered, vec!["beta", "gamma"]);
        assert!(reg
            .mailbox_router
            .get(&alpha)
            .await
            .unwrap()
            .drain()
            .is_empty());
        for recipient in [beta, gamma] {
            let messages = reg.mailbox_router.get(&recipient).await.unwrap().drain();
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].content, "status update");
            assert!(matches!(messages[0].from, MessageSender::Teammate(id) if id == alpha));
        }
    }

    #[tokio::test]
    async fn team_registry_handle_lists_workers() {
        let reg = TeamRegistry::new(AgentId::new());
        let alpha = reg
            .spawn_worker("explorer".into(), "alpha".into(), String::new())
            .await
            .unwrap();
        let _beta = reg
            .spawn_worker("writer".into(), "beta".into(), String::new())
            .await
            .unwrap();

        // Drive one worker to a non-default status so the label mapping is exercised.
        reg.update_status(
            &alpha,
            WorkerStatus::Working {
                activity: "running".into(),
            },
        )
        .await;
        reg.set_team_name(Some("squad".into())).await;

        let handle: &dyn TeamRegistryHandle = &reg;
        let mut workers = handle.list_workers().await;
        assert_eq!(workers.len(), 2);

        // Order is map-iteration-dependent; sort by name for deterministic asserts.
        workers.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(workers[0].name, "alpha");
        assert_eq!(workers[0].agent_type, "explorer");
        assert_eq!(workers[0].agent_id, alpha.to_string());
        assert_eq!(workers[0].status, "working");

        assert_eq!(workers[1].name, "beta");
        assert_eq!(workers[1].agent_type, "writer");
        // Freshly spawned, untouched → Idle.
        assert_eq!(workers[1].status, "idle");

        assert_eq!(handle.team_name().await, Some("squad".to_string()));
    }

    #[test]
    fn worker_status_label_covers_all_variants() {
        assert_eq!(worker_status_label(&WorkerStatus::Idle), "idle");
        assert_eq!(
            worker_status_label(&WorkerStatus::Working {
                activity: "x".into()
            }),
            "working"
        );
        assert_eq!(
            worker_status_label(&WorkerStatus::AwaitingMessage),
            "awaiting_message"
        );
        assert_eq!(worker_status_label(&WorkerStatus::Completed), "completed");
        assert_eq!(
            worker_status_label(&WorkerStatus::Failed { error: "e".into() }),
            "failed"
        );
        assert_eq!(worker_status_label(&WorkerStatus::Killed), "killed");
    }
}
