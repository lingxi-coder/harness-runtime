//! Handback journal, cancellation and recipient-boundary regression tests.

use crate::output_manager::TaskOutputManager;
use crate::registry::TaskRegistry;
use crate::state::{
    InProcessTeammateTaskState, LocalAgentTaskState, TaskState, TaskStateBase, TaskStatus,
};
use crate::TaskType;
use async_trait::async_trait;
use lingxi_core::host::filesystem::{FileContent, FileEvent, FileSystem, FlockGuard, FsError};
use lingxi_core::host::handback::*;
use lingxi_core::host::task_registry::{TaskMessageReceiver, TaskRegistryError};
use lingxi_core::types::{AgentId, MessageId, SessionId};
use std::collections::HashMap;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use test_harness::mocks::MockRuntimeSpawner;
use tokio::sync::{mpsc, Notify};

const DEADLINE: Duration = Duration::from_secs(2);

#[derive(Default)]
struct JournalFs {
    files: Mutex<HashMap<String, String>>,
    fail_atomic: AtomicBool,
    block_atomic: AtomicBool,
    atomic_entered: Notify,
    atomic_release: Notify,
    atomic_commits: AtomicUsize,
}

#[async_trait]
impl FileSystem for JournalFs {
    async fn read_file(
        &self,
        path: &str,
        _offset: Option<u64>,
        _limit: Option<u64>,
    ) -> Result<FileContent, FsError> {
        let content = self
            .files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .ok_or_else(|| FsError::NotFound(path.into()))?;
        Ok(FileContent {
            total_lines: content.lines().count() as u64,
            content,
            truncated: false,
        })
    }

    async fn write_file(&self, path: &str, content: &str) -> Result<(), FsError> {
        self.files
            .lock()
            .unwrap()
            .insert(path.into(), content.into());
        Ok(())
    }

    async fn write_file_rooted_atomic(
        &self,
        root: &Path,
        relative: &Path,
        content: &str,
    ) -> Result<(), FsError> {
        let path = lingxi_core::host::rooted_fs::checked_join(root, relative)?;
        if self.fail_atomic.swap(false, Ordering::AcqRel) {
            return Err(FsError::Io("injected journal failure".into()));
        }
        if self.block_atomic.swap(false, Ordering::AcqRel) {
            self.atomic_entered.notify_one();
            self.atomic_release.notified().await;
        }
        self.write_file(&path.to_string_lossy(), content).await?;
        self.atomic_commits.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn is_within_workspace(&self, _: &str) -> bool {
        true
    }

    async fn watch(
        &self,
        _: &str,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = FileEvent> + Send>>, FsError> {
        Err(FsError::Io("watch is unused in journal tests".into()))
    }

    async fn append_file(&self, path: &str, content: &str) -> Result<(), FsError> {
        self.files
            .lock()
            .unwrap()
            .entry(path.into())
            .or_default()
            .push_str(content);
        Ok(())
    }

    async fn truncate(&self, path: &str, len: u64) -> Result<(), FsError> {
        if let Some(content) = self.files.lock().unwrap().get_mut(path) {
            content.truncate(len as usize);
        }
        Ok(())
    }

    async fn file_mtime(&self, _: &str) -> Result<SystemTime, FsError> {
        Ok(SystemTime::UNIX_EPOCH)
    }

    async fn file_size(&self, path: &str) -> Result<u64, FsError> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .map(|content| content.len() as u64)
            .ok_or_else(|| FsError::NotFound(path.into()))
    }

    async fn delete_file(&self, path: &str) -> Result<(), FsError> {
        self.files.lock().unwrap().remove(path);
        Ok(())
    }

    async fn symlink(&self, _: &str, _: &str) -> Result<(), FsError> {
        Err(FsError::Io("symlinks are unused in journal tests".into()))
    }

    async fn flock_exclusive(&self, _: &str) -> Result<Box<dyn FlockGuard>, FsError> {
        Err(FsError::Io("flock is unused in journal tests".into()))
    }

    async fn fsync(&self, _: &str) -> Result<(), FsError> {
        Ok(())
    }
}

struct MainAdmission {
    scope: Mutex<HandbackSessionScope>,
    accepted: Mutex<Vec<HandbackEnvelope>>,
}

#[async_trait]
impl ReportingAdmission for MainAdmission {
    async fn main_scope(&self) -> Option<HandbackSessionScope> {
        Some(*self.scope.lock().unwrap())
    }

    async fn admit(&self, envelope: HandbackEnvelope) -> Result<(), HandbackAdmissionError> {
        if !envelope.validate()
            || envelope.receipt.recipient
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

fn make_registry(
    root: &Path,
    fs: Arc<JournalFs>,
    scope: HandbackSessionScope,
) -> (Arc<TaskRegistry>, Arc<MainAdmission>) {
    let output = Arc::new(TaskOutputManager::new(root.to_path_buf(), fs.clone()));
    let registry = Arc::new(TaskRegistry::new(
        Arc::new(MockRuntimeSpawner::default()),
        fs,
        output,
    ));
    registry.bind_self(&registry);
    let admission = Arc::new(MainAdmission {
        scope: Mutex::new(scope),
        accepted: Mutex::new(Vec::new()),
    });
    assert!(registry.bind_reporting_admission(Arc::downgrade(
        &(admission.clone() as Arc<dyn ReportingAdmission>),
    )));
    (registry, admission)
}

fn scope() -> HandbackSessionScope {
    HandbackSessionScope {
        session_id: SessionId::new(),
        activation_epoch: 1,
    }
}

fn base(id: &str, task_type: TaskType, status: TaskStatus) -> TaskStateBase {
    TaskStateBase {
        id: id.into(),
        task_type,
        status,
        description: "handback boundary test".into(),
        tool_use_id: None,
        start_time: SystemTime::now(),
        end_time: None,
        total_paused_ms: 0,
        output_file: std::path::PathBuf::from(format!("/tmp/tasks/{id}.output")),
        evict_after: None,
        output_offset: 0,
        notified: false,
        creator_teammate_name: None,
        creator_team_name: None,
        creator_agent_id: None,
    }
}

fn local(id: &str, agent_id: AgentId, status: TaskStatus) -> LocalAgentTaskState {
    LocalAgentTaskState {
        handback: None,
        handback_history: Vec::new(),

        agent_spawn_provenance: Default::default(),
        agent_list_lifecycle: Default::default(),
        spawned_description: None,
        is_parked: status == TaskStatus::Completed,
        is_observer: false,
        observed_agent_id: None,
        base: base(id, TaskType::LocalAgent, status),
        agent_id,
        subagent_type: "worker".into(),
        prompt: String::new(),
        error: None,
        messages: Vec::new(),
        pending_messages: Vec::new(),
        is_backgrounded: false,
        outcome: Default::default(),
        forked_skill_name: None,
    }
}

async fn add_local(registry: &TaskRegistry, id: &str, actor: AgentId) {
    registry
        .insert_state_for_test(TaskState::LocalAgent(local(id, actor, TaskStatus::Running)))
        .await;
}

fn start(actor: AgentId, scope: HandbackSessionScope, caller: Option<AgentId>) -> BeginHandbackRun {
    BeginHandbackRun {
        agent_id: actor,
        scope,
        active: true,
        caller: caller.map(|agent_id| HandbackRecipient::Agent { scope, agent_id }),
        resumer: HandbackRecipient::Main { scope },
        restored_state: None,
        restored_history: Vec::new(),
    }
}

fn report(actor: AgentId) -> PreparedHandbackReport {
    PreparedHandbackReport {
        message_id: MessageId::new(),
        report: HandbackReport {
            text: "whole sanitized report".into(),
            warning: None,
        },
        body: handback_frame("whole sanitized report"),
        body_utf16: None,
        sender_name: "worker".into(),
        sender_id: actor.to_string(),
        sender_task_id: actor.to_string(),
        agent_type: "worker".into(),
        flagged: false,
    }
}

fn admitted(outcome: HandbackAdmissionOutcome) -> HandbackReceipt {
    match outcome {
        HandbackAdmissionOutcome::Admitted(receipt) => receipt,
        other => panic!("expected admitted report, got {other:?}"),
    }
}

async fn journal(
    fs: &JournalFs,
    root: &Path,
    scope: HandbackSessionScope,
    recipient: AgentId,
) -> serde_json::Value {
    let relative = format!(
        "{}-{}.handback-inbox.json",
        scope.session_id.as_uuid().simple(),
        recipient.as_uuid().simple()
    );
    let file = fs
        .read_file_rooted_no_follow(root, Path::new(&relative))
        .await
        .unwrap();
    serde_json::from_str(&file.content).unwrap()
}

#[tokio::test]
async fn handback_nested_failed_durable_write_and_ack_retry_preserve_allowance_and_exact_receipt() {
    let root = tempfile::tempdir().unwrap();
    let fs = Arc::new(JournalFs::default());
    let scope = scope();
    let (registry, main) = make_registry(root.path(), fs.clone(), scope);
    let caller = AgentId::new();
    let actor = AgentId::new();
    add_local(&registry, "caller", caller).await;
    add_local(&registry, "sender", actor).await;
    let token = registry
        .begin_handback_run(start(actor, scope, Some(caller)))
        .await
        .unwrap();
    let mut report = report(actor);
    report.sender_task_id = "untrusted-task-label".into();
    fs.fail_atomic.store(true, Ordering::Release);
    assert_eq!(
        registry.try_deliver_handback(&token, report.clone()).await,
        HandbackAdmissionOutcome::Rejected
    );
    assert!(registry
        .handback_state(&token)
        .await
        .unwrap()
        .receipt
        .is_none());
    assert!(registry
        .pending_handback_reports_for(caller)
        .await
        .is_empty());
    assert!(main.accepted.lock().unwrap().is_empty());
    let receipt = admitted(registry.try_deliver_handback(&token, report.clone()).await);
    assert_eq!(receipt.message_id, report.message_id);
    let pending = registry.pending_handback_reports_for(caller).await;
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].origin.sender_task_id, actor.to_string());
    assert_eq!(pending[0].origin.from, actor.to_string());
    assert!(pending[0].validate());
    assert_eq!(
        registry.try_deliver_handback(&token, report).await,
        HandbackAdmissionOutcome::Duplicate
    );
    fs.fail_atomic.store(true, Ordering::Release);
    assert!(
        !registry
            .acknowledge_handback_consumption(caller, &receipt)
            .await
    );
    let persisted = journal(&fs, root.path(), scope, caller).await;
    assert_eq!(persisted["pending"].as_array().unwrap().len(), 1);
    assert!(persisted["consumed"].as_array().unwrap().is_empty());
    assert_eq!(registry.pending_handback_reports_for(caller).await.len(), 1);
    assert!(
        registry
            .acknowledge_handback_consumption(caller, &receipt)
            .await
    );
    assert!(
        registry
            .acknowledge_handback_consumption(caller, &receipt)
            .await
    );
    let persisted = journal(&fs, root.path(), scope, caller).await;
    assert!(persisted["pending"].as_array().unwrap().is_empty());
    assert_eq!(persisted["consumed"], serde_json::json!([receipt]));
    assert_eq!(fs.atomic_commits.load(Ordering::Acquire), 2);
}

#[tokio::test]
async fn handback_owned_consumption_ack_settles_after_caller_abort_and_survives_cold_restore() {
    let root = tempfile::tempdir().unwrap();
    let fs = Arc::new(JournalFs::default());
    let original_scope = scope();
    let (registry, _main_admission_owner) = make_registry(root.path(), fs.clone(), original_scope);
    let caller = AgentId::new();
    let actor = AgentId::new();
    add_local(&registry, "caller", caller).await;
    add_local(&registry, "sender", actor).await;
    let caller_token = registry
        .begin_handback_run(start(caller, original_scope, None))
        .await
        .unwrap();
    let caller_state = registry.handback_state(&caller_token).await.unwrap();
    let token = registry
        .begin_handback_run(start(actor, original_scope, Some(caller)))
        .await
        .unwrap();
    let receipt = admitted(registry.try_deliver_handback(&token, report(actor)).await);
    fs.block_atomic.store(true, Ordering::Release);
    let ack = {
        let registry = registry.clone();
        let receipt = receipt.clone();
        tokio::spawn(async move {
            registry
                .acknowledge_handback_consumption(caller, &receipt)
                .await
        })
    };
    tokio::time::timeout(DEADLINE, fs.atomic_entered.notified())
        .await
        .expect("ACK must reach the blocked journal commit");
    assert_eq!(registry.pending_handback_reports_for(caller).await.len(), 1);
    ack.abort();
    assert!(ack.await.unwrap_err().is_cancelled());
    fs.atomic_release.notify_one();
    tokio::time::timeout(DEADLINE, async {
        while !registry
            .pending_handback_reports_for(caller)
            .await
            .is_empty()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("recipient-owned ACK must settle after its caller disappears");
    assert_eq!(fs.atomic_commits.load(Ordering::Acquire), 2);
    let next_scope = HandbackSessionScope {
        activation_epoch: 2,
        ..original_scope
    };
    let (cold, _cold_admission_owner) = make_registry(root.path(), fs.clone(), next_scope);
    add_local(&cold, "caller-restored", caller).await;
    let mut input = start(caller, next_scope, None);
    input.restored_state = Some(caller_state);
    cold.begin_handback_run(input).await.unwrap();
    assert!(cold.pending_handback_reports_for(caller).await.is_empty());
    assert!(
        cold.acknowledge_handback_consumption(caller, &receipt)
            .await
    );
    let wrong = HandbackReceipt {
        message_id: MessageId::new(),
        ..receipt.clone()
    };
    assert!(!cold.acknowledge_handback_consumption(caller, &wrong).await);
    let persisted = journal(&fs, root.path(), next_scope, caller).await;
    assert_eq!(persisted["consumed"], serde_json::json!([receipt]));
}

struct FullChannelReceiver {
    channel: mpsc::Sender<HandbackEnvelope>,
    entered: Notify,
}

#[async_trait]
impl TaskMessageReceiver for FullChannelReceiver {
    async fn send(&self, _: String) -> Result<(), TaskRegistryError> {
        panic!("peer wake must preserve its typed authority")
    }

    async fn send_peer(&self, envelope: HandbackEnvelope) -> Result<(), TaskRegistryError> {
        self.entered.notify_one();
        self.channel
            .send(envelope)
            .await
            .map_err(|_| TaskRegistryError::Internal("receiver stopped".into()))
    }
}

#[tokio::test]
async fn handback_actual_full_hundred_event_channel_cannot_hold_admission_or_user_stop() {
    let root = tempfile::tempdir().unwrap();
    let fs = Arc::new(JournalFs::default());
    let scope = scope();
    let (registry, _main_admission_owner) = make_registry(root.path(), fs, scope);
    let caller = AgentId::new();
    let actor = AgentId::new();
    add_local(&registry, "caller", caller).await;
    add_local(&registry, "sender", actor).await;
    let token = registry
        .begin_handback_run(start(actor, scope, Some(caller)))
        .await
        .unwrap();
    let (channel, receiver) = mpsc::channel(100);
    let filler = report(actor).envelope(
        token.run(),
        HandbackRecipient::Agent {
            scope,
            agent_id: caller,
        },
    );
    assert!(filler.validate());
    for _ in 0..100 {
        channel.try_send(filler.clone()).unwrap();
    }
    assert_eq!(channel.capacity(), 0);
    let wake = Arc::new(FullChannelReceiver {
        channel,
        entered: Notify::new(),
    });
    registry
        .bind_agent_message_receiver("caller", wake.clone())
        .await
        .unwrap();
    let receipt = admitted(
        tokio::time::timeout(
            DEADLINE,
            registry.try_deliver_handback(&token, report(actor)),
        )
        .await
        .expect("durable admission cannot wait for a full caller event queue"),
    );
    tokio::time::timeout(DEADLINE, wake.entered.notified())
        .await
        .unwrap();
    assert_eq!(registry.pending_handback_reports_for(caller).await.len(), 1);
    let forged = HandbackRunToken::mint(token.run());
    assert_eq!(
        tokio::time::timeout(
            DEADLINE,
            registry.try_deliver_handback(&forged, report(actor))
        )
        .await
        .expect("blocked wake cannot hold the registry's global report transaction"),
        HandbackAdmissionOutcome::StaleRun
    );
    tokio::time::timeout(DEADLINE, registry.kill_with_reason("sender", "user"))
        .await
        .expect("user stop must cross the report transaction while wake is blocked")
        .unwrap();
    assert!(registry.handback_state(&token).await.is_none());
    let stopped = registry.handback_state_for_agent(actor).await.unwrap();
    assert_eq!(stopped.receipt, Some(receipt));
    assert!(!stopped.active);
    assert_eq!(
        registry.try_deliver_handback(&token, report(actor)).await,
        HandbackAdmissionOutcome::StaleRun
    );
    drop(receiver);
}

#[tokio::test]
async fn handback_readability_requires_local_running_or_completed_with_owned_work() {
    for case in ["running", "owned", "idle", "teammate", "stopped"] {
        let root = tempfile::tempdir().unwrap();
        let scope = scope();
        let (registry, _main_admission_owner) =
            make_registry(root.path(), Arc::new(JournalFs::default()), scope);
        let caller = AgentId::new();
        let actor = AgentId::new();
        add_local(&registry, "sender", actor).await;
        if case == "teammate" {
            registry
                .insert_state_for_test(TaskState::InProcessTeammate(InProcessTeammateTaskState {
                    child_model: None,
                    child_model_profile: None,
                    handback: None,
                    handback_history: Vec::new(),

                    agent_spawn_provenance: Default::default(),
                    spawned_agent_type: None,
                    spawned_description: None,
                    is_idle: false,
                    awaiting_plan_approval: false,
                    base: base("caller", TaskType::InProcessTeammate, TaskStatus::Running),
                    agent_id: caller,
                    pending_messages: Vec::new(),
                }))
                .await;
        } else {
            let status = if case == "running" {
                TaskStatus::Running
            } else {
                TaskStatus::Completed
            };
            let mut row = local("caller", caller, status);
            if case == "stopped" {
                row.outcome.killed_by = Some("user".into());
            }
            registry
                .insert_state_for_test(TaskState::LocalAgent(row))
                .await;
            if matches!(case, "owned" | "stopped") {
                let mut owned = local("owned-job", AgentId::new(), TaskStatus::Running);
                owned.is_backgrounded = true;
                owned.base.creator_agent_id = Some(caller);
                registry
                    .insert_state_for_test(TaskState::LocalAgent(owned))
                    .await;
            }
        }
        let token = registry
            .begin_handback_run(start(actor, scope, Some(caller)))
            .await
            .unwrap();
        let expected = if matches!(case, "running" | "owned") {
            HandbackRecipient::Agent {
                scope,
                agent_id: caller,
            }
        } else {
            HandbackRecipient::Main { scope }
        };
        assert_eq!(
            registry.handback_state(&token).await.unwrap().recipient,
            expected,
            "{case}"
        );
    }
}

#[tokio::test]
async fn handback_cold_registered_recipient_from_another_session_is_not_rebound() {
    let root = tempfile::tempdir().unwrap();
    let current = scope();
    let other_session = scope();
    let (registry, _main_admission_owner) =
        make_registry(root.path(), Arc::new(JournalFs::default()), current);
    let caller = AgentId::new();
    let actor = AgentId::new();
    add_local(&registry, "caller", caller).await;
    add_local(&registry, "sender", actor).await;
    let old_run = HandbackRunKey {
        scope: other_session,
        agent_id: actor,
        run_epoch: 7,
    };
    let old_recipient = HandbackRecipient::Agent {
        scope: other_session,
        agent_id: caller,
    };
    let mut old = HandbackState::new_run(
        old_run,
        true,
        None,
        Some(old_recipient),
        HandbackRecipient::Main {
            scope: other_session,
        },
        |_| true,
    );
    old.receipt = Some(HandbackReceipt {
        run: old_run,
        recipient: old_recipient,
        message_id: MessageId::new(),
    });
    let mut input = start(actor, current, Some(caller));
    input.restored_state = Some(old.clone());
    let token = registry.begin_handback_run(input).await.unwrap();
    let state = registry.handback_state(&token).await.unwrap();
    assert_eq!(state.recipient, HandbackRecipient::Main { scope: current });
    assert_eq!(state.run.run_epoch, 8);
    assert!(state.receipt.is_none());
    assert_eq!(registry.handback_history_for_agent(actor).await, vec![old]);
    let forged = HandbackRunToken::mint(token.run());
    assert_eq!(
        registry.try_deliver_handback(&forged, report(actor)).await,
        HandbackAdmissionOutcome::StaleRun
    );
}

#[tokio::test]
async fn handback_live_token_cannot_report_after_main_session_activation_changes() {
    let root = tempfile::tempdir().unwrap();
    let original = scope();
    let (registry, main) = make_registry(root.path(), Arc::new(JournalFs::default()), original);
    let actor = AgentId::new();
    add_local(&registry, "sender", actor).await;
    let token = registry
        .begin_handback_run(start(actor, original, None))
        .await
        .unwrap();
    let next_activation = HandbackSessionScope {
        activation_epoch: original.activation_epoch + 1,
        ..original
    };
    for replacement in [next_activation, scope()] {
        *main.scope.lock().unwrap() = replacement;
        assert_eq!(
            registry.try_deliver_handback(&token, report(actor)).await,
            HandbackAdmissionOutcome::StaleRun
        );
        assert!(registry
            .handback_state(&token)
            .await
            .unwrap()
            .receipt
            .is_none());
        assert!(main.accepted.lock().unwrap().is_empty());
        assert!(registry
            .begin_handback_run(start(actor, original, None))
            .await
            .is_err());
    }
}

#[tokio::test]
async fn handback_gone_caller_rejects_foreground_and_retargets_only_background_report() {
    for backgrounded in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let scope = scope();
        let (registry, main) = make_registry(root.path(), Arc::new(JournalFs::default()), scope);
        let caller = AgentId::new();
        let actor = AgentId::new();
        add_local(&registry, "caller", caller).await;
        let mut sender = local("sender", actor, TaskStatus::Running);
        sender.is_backgrounded = backgrounded;
        sender.base.creator_agent_id = Some(caller);
        registry
            .insert_state_for_test(TaskState::LocalAgent(sender))
            .await;
        let token = registry
            .begin_handback_run(start(actor, scope, Some(caller)))
            .await
            .unwrap();
        registry.kill_with_reason("caller", "user").await.unwrap();
        let outcome = registry.try_deliver_handback(&token, report(actor)).await;
        if backgrounded {
            let receipt = admitted(outcome);
            assert_eq!(receipt.recipient, HandbackRecipient::Main { scope });
            assert_eq!(main.accepted.lock().unwrap().len(), 1);
            assert_eq!(
                registry.handback_state(&token).await.unwrap().recipient,
                HandbackRecipient::Main { scope }
            );
        } else {
            assert_eq!(outcome, HandbackAdmissionOutcome::CallerGone);
            assert!(main.accepted.lock().unwrap().is_empty());
            assert!(registry
                .handback_state(&token)
                .await
                .unwrap()
                .receipt
                .is_none());
        }
        assert_eq!(
            registry
                .get("sender")
                .await
                .unwrap()
                .base()
                .creator_agent_id,
            Some(caller),
            "fallback must not rewrite creator ancestry"
        );
    }
}
