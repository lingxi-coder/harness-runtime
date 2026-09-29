//! End-to-end tests for the coordinator mailbox router.
//!
//! See Plan 07 — Task 5.

use coordinator::{MailboxError, MessageSender, TeamRegistry, TeammateMessage};
use lingxi_core::types::AgentId;

#[tokio::test]
async fn coordinator_routes_message_to_worker() {
    let coord = AgentId::new();
    let team = TeamRegistry::new(coord);
    let worker = team
        .spawn_worker("explorer".into(), "explore-1".into(), "task-1".into())
        .await
        .unwrap();

    let msg = TeammateMessage {
        from: MessageSender::Coordinator,
        from_name: "team-lead".into(),
        content: "go".into(),
        summary: None,
        message_id: "m1".into(),
        timestamp: std::time::SystemTime::now(),
        request_id: None,
    };
    team.mailbox_router
        .route(&worker, msg.clone())
        .await
        .unwrap();

    // The worker's mailbox is registered in the router; routing succeeded.
    // (Drain test belongs to the worker side — covered in Plan 09 SkillTool tests.)
}

#[tokio::test]
async fn route_to_unknown_worker_errors() {
    let coord = AgentId::new();
    let team = TeamRegistry::new(coord);
    let r = team
        .mailbox_router
        .route(
            &AgentId::new(),
            TeammateMessage {
                from: MessageSender::Coordinator,
                from_name: "team-lead".into(),
                content: String::new(),
                summary: None,
                message_id: String::new(),
                timestamp: std::time::SystemTime::now(),
                request_id: None,
            },
        )
        .await;
    assert!(matches!(r, Err(MailboxError::NotFound(_))));
}
