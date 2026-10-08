//! Executed task-boundary tests; native body/schema helpers have separate oracle tests.
use super::*;
use lingxi_core::host::handback::*;
use lingxi_core::types::{AgentId, MessageId, SessionId};

#[path = "handback_lifecycle_test.rs"]
mod lifecycle;

struct Admission {
    scope: StdMutex<HandbackSessionScope>,
    reject: std::sync::atomic::AtomicBool,
    blocked: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    accepted: StdMutex<Vec<HandbackEnvelope>>,
}

#[async_trait]
impl ReportingAdmission for Admission {
    async fn main_scope(&self) -> Option<HandbackSessionScope> {
        Some(*self.scope.lock().unwrap())
    }
    async fn admit(&self, envelope: HandbackEnvelope) -> Result<(), HandbackAdmissionError> {
        self.entered.notify_one();
        if self.blocked.load(Ordering::Acquire) {
            self.release.notified().await;
        }
        if self.reject.load(Ordering::Acquire) {
            return Err(HandbackAdmissionError::Rejected {
                reason: "recipient denied admission".into(),
            });
        }
        if envelope.receipt.recipient
            != (HandbackRecipient::Main {
                scope: *self.scope.lock().unwrap(),
            })
        {
            return Err(HandbackAdmissionError::StaleScope);
        }
        self.accepted.lock().unwrap().push(envelope);
        Ok(())
    }
}

fn scope(epoch: u64) -> HandbackSessionScope {
    HandbackSessionScope {
        session_id: SessionId::new(),
        activation_epoch: epoch,
    }
}

fn new_admission(scope: HandbackSessionScope) -> Arc<Admission> {
    Arc::new(Admission {
        scope: StdMutex::new(scope),
        reject: false.into(),
        blocked: false.into(),
        entered: Default::default(),
        release: Default::default(),
        accepted: Default::default(),
    })
}

async fn actor_row(
    registry: &TaskRegistry,
    task_id: &str,
    actor: AgentId,
    background: bool,
    creator: Option<AgentId>,
) {
    let mut row = agent_state(task_id, TaskStatus::Running);
    let TaskState::LocalAgent(agent) = &mut row else {
        unreachable!()
    };
    agent.agent_id = actor;
    agent.is_backgrounded = background;
    agent.base.creator_agent_id = creator;
    registry.insert_state_for_test(row).await;
}

async fn begin(
    registry: &TaskRegistry,
    actor: AgentId,
    scope: HandbackSessionScope,
    caller: Option<HandbackRecipient>,
) -> HandbackRunToken {
    registry
        .begin_handback_run(BeginHandbackRun {
            agent_id: actor,
            scope,
            active: true,
            caller,
            resumer: HandbackRecipient::Main { scope },
            restored_state: None,
            restored_history: Vec::new(),
        })
        .await
        .unwrap()
}

fn report() -> PreparedHandbackReport {
    PreparedHandbackReport {
        message_id: MessageId::new(),
        report: HandbackReport {
            text: "entire sanitized report".into(),
            warning: None,
        },
        body: handback_frame("persisted report pointer"),
        body_utf16: None,
        sender_name: "worker".into(),
        sender_id: "worker".into(),
        sender_task_id: "model-forged-task".into(),
        agent_type: "worker".into(),
        flagged: false,
    }
}

#[tokio::test]
async fn handback_rejection_retry_duplicate_and_new_run_reset() {
    let (_dir, registry) = make_registry();
    let registry = Arc::new(registry);
    registry.bind_self(&registry);
    let scope = scope(1);
    let admission = new_admission(scope);
    assert!(registry.bind_reporting_admission(Arc::downgrade(
        &(admission.clone() as Arc<dyn ReportingAdmission>),
    )));
    let actor = AgentId::new();
    actor_row(&registry, "child", actor, false, None).await;
    let token = begin(&registry, actor, scope, None).await;
    admission.reject.store(true, Ordering::Release);
    let prepared = report();
    assert_eq!(
        registry
            .try_deliver_handback(&token, prepared.clone())
            .await,
        HandbackAdmissionOutcome::Rejected
    );
    assert!(registry
        .handback_state(&token)
        .await
        .unwrap()
        .receipt
        .is_none());
    assert_eq!(registry.next_handback_bounce(&token).await, Some(1));
    admission.reject.store(false, Ordering::Release);
    let HandbackAdmissionOutcome::Admitted(receipt) = registry
        .try_deliver_handback(&token, prepared.clone())
        .await
    else {
        panic!("retry must admit")
    };
    assert_eq!(receipt.message_id, prepared.message_id);
    assert_eq!(
        registry.try_deliver_handback(&token, report()).await,
        HandbackAdmissionOutcome::Duplicate
    );
    assert_eq!(registry.next_handback_bounce(&token).await, None);
    assert_eq!(
        admission.accepted.lock().unwrap()[0].origin.sender_task_id,
        actor.to_string()
    );
    assert_eq!(
        registry
            .handback_state(&token)
            .await
            .unwrap()
            .report
            .unwrap()
            .text,
        "entire sanitized report"
    );
    let next = begin(&registry, actor, scope, None).await;
    assert_ne!(token, next);
    let state = registry.handback_state(&next).await.unwrap();
    assert!(state.receipt.is_none() && state.report.is_none() && state.disposition.is_none());
    assert_eq!(state.bounce_count, 0);
    assert_eq!(
        registry.handback_history_for_agent(actor).await[0].receipt,
        Some(receipt)
    );
    assert_eq!(
        registry.try_deliver_handback(&token, report()).await,
        HandbackAdmissionOutcome::StaleRun
    );
}

#[tokio::test]
async fn handback_nested_journal_exact_ack_and_cold_recovery() {
    let (_dir, registry) = make_registry();
    let registry = Arc::new(registry);
    registry.bind_self(&registry);
    let scope = scope(1);
    let admission = new_admission(scope);
    registry.bind_reporting_admission(Arc::downgrade(
        &(admission.clone() as Arc<dyn ReportingAdmission>),
    ));
    let parent = AgentId::new();
    let child = AgentId::new();
    actor_row(&registry, "parent", parent, true, None).await;
    actor_row(&registry, "child", child, false, Some(parent)).await;
    let token = begin(
        &registry,
        child,
        scope,
        Some(HandbackRecipient::Agent {
            scope,
            agent_id: parent,
        }),
    )
    .await;
    let HandbackAdmissionOutcome::Admitted(receipt) =
        registry.try_deliver_handback(&token, report()).await
    else {
        panic!("nested queue must admit")
    };
    assert!(admission.accepted.lock().unwrap().is_empty());
    let pending = registry.pending_handback_reports_for(parent).await;
    assert_eq!(pending.len(), 1);
    assert_eq!(
        registry.pending_handback_reports_for(parent).await,
        pending,
        "snapshot must retain queue ownership"
    );
    let path = TaskRegistry::handback_scope(&registry).await.unwrap();
    let journal_name = format!(
        "{}-{}.handback-inbox.json",
        path.session_id.as_uuid().simple(),
        parent.as_uuid().simple()
    );
    let disk = registry
        .fs
        .read_file_rooted_no_follow(
            registry.output_manager.output_dir(),
            std::path::Path::new(&journal_name),
        )
        .await
        .unwrap();
    let journal: serde_json::Value = serde_json::from_str(&disk.content).unwrap();
    assert_eq!(
        journal["pending"].as_array().unwrap().len(),
        1,
        "receipt requires a real durable queue entry"
    );
    let next_scope = HandbackSessionScope {
        activation_epoch: 2,
        ..scope
    };
    let fs = registry.fs.clone();
    let output = Arc::new(TaskOutputManager::new(
        registry.output_manager.output_dir().to_path_buf(),
        fs.clone(),
    ));
    let cold = Arc::new(TaskRegistry::new(
        Arc::new(MockRuntimeSpawner::default()),
        fs,
        output,
    ));
    cold.bind_self(&cold);
    let cold_admission = new_admission(next_scope);
    cold.bind_reporting_admission(Arc::downgrade(
        &(cold_admission.clone() as Arc<dyn ReportingAdmission>),
    ));
    actor_row(&cold, "parent-restored", parent, true, None).await;
    let restored = HandbackState::new_run(
        HandbackRunKey {
            scope,
            agent_id: parent,
            run_epoch: 1,
        },
        true,
        None,
        None,
        HandbackRecipient::Main { scope },
        |_| true,
    );
    cold.begin_handback_run(BeginHandbackRun {
        agent_id: parent,
        scope: next_scope,
        active: true,
        caller: None,
        resumer: HandbackRecipient::Main { scope: next_scope },
        restored_state: Some(restored),
        restored_history: Vec::new(),
    })
    .await
    .unwrap();
    assert_eq!(
        cold.pending_handback_reports_for(parent).await,
        pending,
        "cold recovery retains original receipt/origin epochs"
    );
    let restored = cold.handback_state_for_agent(parent).await.unwrap();
    let resumed_scope = HandbackSessionScope {
        activation_epoch: 3,
        ..scope
    };
    *cold_admission.scope.lock().unwrap() = resumed_scope;
    assert!(
        cold.pending_handback_reports_for(parent).await.is_empty(),
        "hot scope switch never implicitly adopts the old queue"
    );
    cold.begin_handback_run(BeginHandbackRun {
        agent_id: parent,
        scope: resumed_scope,
        active: true,
        caller: None,
        resumer: HandbackRecipient::Main {
            scope: resumed_scope,
        },
        restored_state: Some(restored),
        restored_history: Vec::new(),
    })
    .await
    .unwrap();
    assert_eq!(
        cold.pending_handback_reports_for(parent).await,
        pending,
        "explicit same-session restoration reopens an existing older journal"
    );
    let wrong = HandbackReceipt {
        message_id: MessageId::new(),
        ..receipt.clone()
    };
    assert!(!cold.acknowledge_handback_consumption(parent, &wrong).await);
    assert_eq!(cold.pending_handback_reports_for(parent).await.len(), 1);
    assert!(
        cold.acknowledge_handback_consumption(parent, &receipt)
            .await
    );
    assert!(cold.pending_handback_reports_for(parent).await.is_empty());
    assert!(
        cold.acknowledge_handback_consumption(parent, &receipt)
            .await,
        "consumption acknowledgment is idempotent"
    );
    cold_admission.scope.lock().unwrap().activation_epoch = 4;
    assert!(
        cold.pending_handback_reports_for(parent).await.is_empty(),
        "hot scope switch does not recover an old inbox implicitly"
    );
}

struct BlockedWake {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait]
impl lingxi_core::host::task_registry::TaskMessageReceiver for BlockedWake {
    async fn send(
        &self,
        _message: String,
    ) -> Result<(), lingxi_core::host::task_registry::TaskRegistryError> {
        panic!("peer report cannot use human/string delivery")
    }
    async fn send_peer(
        &self,
        _envelope: HandbackEnvelope,
    ) -> Result<(), lingxi_core::host::task_registry::TaskRegistryError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}

#[tokio::test]
async fn handback_full_wakeup_queue_does_not_block_sync_parent_or_stop() {
    let (_dir, registry) = make_registry();
    let registry = Arc::new(registry);
    registry.bind_self(&registry);
    let scope = scope(1);
    let admission = new_admission(scope);
    registry.bind_reporting_admission(Arc::downgrade(
        &(admission.clone() as Arc<dyn ReportingAdmission>),
    ));
    let parent = AgentId::new();
    let child = AgentId::new();
    actor_row(&registry, "parent", parent, false, None).await;
    actor_row(&registry, "child", child, false, Some(parent)).await;
    let wake = Arc::new(BlockedWake {
        entered: Default::default(),
        release: Default::default(),
    });
    registry
        .bind_agent_message_receiver("parent", wake.clone())
        .await
        .unwrap();
    let token = begin(
        &registry,
        child,
        scope,
        Some(HandbackRecipient::Agent {
            scope,
            agent_id: parent,
        }),
    )
    .await;
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        registry.try_deliver_handback(&token, report()),
    )
    .await
    .unwrap();
    assert!(matches!(result, HandbackAdmissionOutcome::Admitted(_)));
    tokio::time::timeout(
        std::time::Duration::from_millis(200),
        wake.entered.notified(),
    )
    .await
    .unwrap();
    assert_eq!(registry.pending_handback_reports_for(parent).await.len(), 1);
    tokio::time::timeout(
        std::time::Duration::from_millis(200),
        registry.invalidate_handback_for_task("child"),
    )
    .await
    .unwrap();
    assert!(registry.handback_state(&token).await.is_none());
    wake.release.notify_one();
}

#[tokio::test]
async fn handback_admission_settles_after_invoking_future_is_dropped() {
    let (_dir, registry) = make_registry();
    let registry = Arc::new(registry);
    registry.bind_self(&registry);
    let scope = scope(1);
    let admission = new_admission(scope);
    admission.blocked.store(true, Ordering::Release);
    registry.bind_reporting_admission(Arc::downgrade(
        &(admission.clone() as Arc<dyn ReportingAdmission>),
    ));
    let actor = AgentId::new();
    actor_row(&registry, "child", actor, false, None).await;
    let token = begin(&registry, actor, scope, None).await;
    let call = {
        let registry = registry.clone();
        let token = token.clone();
        tokio::spawn(async move { registry.try_deliver_handback(&token, report()).await })
    };
    admission.entered.notified().await;
    call.abort();
    let _ = call.await;
    admission.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if registry
                .handback_state(&token)
                .await
                .is_some_and(|state| state.receipt.is_some())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(admission.accepted.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn handback_old_completion_blocked_on_io_cannot_park_a_new_run() {
    let (_dir, registry) = make_registry();
    let registry = Arc::new(registry);
    registry.bind_self(&registry);
    let scope = scope(1);
    let admission = new_admission(scope);
    registry.bind_reporting_admission(Arc::downgrade(
        &(admission.clone() as Arc<dyn ReportingAdmission>),
    ));
    let actor = AgentId::new();
    actor_row(&registry, "parent", actor, true, None).await;
    let old = begin(&registry, actor, scope, None).await;
    let old_state = registry.handback_state(&old).await.unwrap();
    let io_entered = Arc::new(tokio::sync::Notify::new());
    let io_release = Arc::new(tokio::sync::Notify::new());
    let completion = {
        let registry = registry.clone();
        let entered = io_entered.clone();
        let release = io_release.clone();
        tokio::spawn(async move {
            entered.notify_one();
            release.notified().await;
            registry
                .set_agent_outcome(
                    "parent",
                    lingxi_core::host::task_registry::AgentTerminalOutcome {
                        handback: Some(old_state.clone()),
                        result: Some("old report".into()),
                        ..Default::default()
                    },
                )
                .await;
            registry
                .mark_task_rested(
                    "parent",
                    Some("old report".into()),
                    None,
                    Some(actor),
                    None,
                    None,
                    Some(old_state.run),
                )
                .await;
        })
    };
    io_entered.notified().await;
    let current = begin(&registry, actor, scope, None).await;
    io_release.notify_one();
    completion.await.unwrap();
    let TaskState::LocalAgent(row) = registry.get("parent").await.unwrap() else {
        unreachable!()
    };
    assert_eq!(row.base.status, TaskStatus::Running);
    assert!(!row.is_parked);
    assert!(row.outcome.result.is_none());
    assert_eq!(row.handback.unwrap().run, current.run());
    assert!(registry.pending_rest.read().await.is_empty());
}

#[tokio::test]
async fn handback_cold_recipient_rebind_retains_real_caller_and_immutable_archive() {
    let (_dir, registry) = make_registry();
    let registry = Arc::new(registry);
    registry.bind_self(&registry);
    let old_scope = scope(1);
    let scope = HandbackSessionScope {
        activation_epoch: 2,
        ..old_scope
    };
    let admission = new_admission(scope);
    registry.bind_reporting_admission(Arc::downgrade(
        &(admission.clone() as Arc<dyn ReportingAdmission>),
    ));
    let parent = AgentId::new();
    let actor = AgentId::new();
    actor_row(&registry, "caller-real-row", parent, true, None).await;
    actor_row(&registry, "child-restored", actor, true, Some(parent)).await;
    let run = HandbackRunKey {
        scope: old_scope,
        agent_id: actor,
        run_epoch: 4,
    };
    let old_recipient = HandbackRecipient::Agent {
        scope: old_scope,
        agent_id: parent,
    };
    let mut old = HandbackState::new_run(
        run,
        true,
        None,
        Some(old_recipient),
        HandbackRecipient::Main { scope: old_scope },
        |_| true,
    );
    old.receipt = Some(HandbackReceipt {
        run,
        recipient: old_recipient,
        message_id: MessageId::new(),
    });
    old.report = Some(HandbackReport {
        text: "old full report".into(),
        warning: Some("old security warning".into()),
    });
    let token = registry
        .begin_handback_run(BeginHandbackRun {
            agent_id: actor,
            scope,
            active: true,
            caller: None,
            resumer: HandbackRecipient::Main { scope },
            restored_state: Some(old.clone()),
            restored_history: Vec::new(),
        })
        .await
        .unwrap();
    let state = registry.handback_state(&token).await.unwrap();
    assert_eq!(
        state.recipient,
        HandbackRecipient::Agent {
            scope,
            agent_id: parent
        }
    );
    assert!(state.receipt.is_none() && state.report.is_none());
    assert_eq!(registry.handback_history_for_agent(actor).await, vec![old]);
    assert_eq!(
        registry
            .get("child-restored")
            .await
            .unwrap()
            .base()
            .creator_agent_id,
        Some(parent)
    );
    registry
        .kill_with_reason("child-restored", "user")
        .await
        .unwrap();
    assert!(
        registry
            .begin_handback_run(BeginHandbackRun {
                agent_id: actor,
                scope,
                active: true,
                caller: None,
                resumer: HandbackRecipient::Main { scope },
                restored_state: None,
                restored_history: Vec::new()
            })
            .await
            .is_err(),
        "peer/startup cannot revive a user-stopped actor"
    );
}
