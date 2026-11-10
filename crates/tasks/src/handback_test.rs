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
use lingxi_core::host::team_spawn::TeamSpawnSeam;
use lingxi_core::types::{AgentId, MessageId, SessionId};
use std::collections::HashMap;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use test_harness::mocks::MockRuntimeSpawner;
use tokio::sync::{mpsc, Notify};

#[path = "handback_native_test.rs"]
mod native;

const DEADLINE: Duration = Duration::from_secs(2);

#[derive(Default)]
struct JournalFs {
    files: Mutex<HashMap<String, String>>,
    fail_atomic: AtomicBool,
    fail_append: AtomicBool,
    partial_append: AtomicBool,
    fail_after_append: AtomicBool,
    fail_truncate: AtomicBool,
    fail_journal_clear: AtomicBool,
    block_append: AtomicBool,
    append_entered: Notify,
    append_release: Notify,
    atomic_commits: AtomicUsize,
    atomic_bytes: AtomicUsize,
    append_commits: AtomicUsize,
    append_bytes: AtomicUsize,
    reads: AtomicUsize,
}

#[async_trait]
impl FileSystem for JournalFs {
    async fn read_file(
        &self,
        path: &str,
        _offset: Option<u64>,
        _limit: Option<u64>,
    ) -> Result<FileContent, FsError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
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
        self.write_file(&path.to_string_lossy(), content).await?;
        self.atomic_commits.fetch_add(1, Ordering::AcqRel);
        self.atomic_bytes.fetch_add(content.len(), Ordering::AcqRel);
        Ok(())
    }

    async fn append_file_rooted_durable(
        &self,
        root: &Path,
        relative: &Path,
        content: &str,
    ) -> Result<(), FsError> {
        let path = lingxi_core::host::rooted_fs::checked_join(root, relative)?;
        if self.fail_append.swap(false, Ordering::AcqRel) {
            return Err(FsError::Io("injected append failure".into()));
        }
        if self.block_append.swap(false, Ordering::AcqRel) {
            self.append_entered.notify_one();
            self.append_release.notified().await;
        }
        if self.partial_append.swap(false, Ordering::AcqRel) {
            let mut end = content.len() / 2;
            while !content.is_char_boundary(end) {
                end -= 1;
            }
            self.append_file(&path.to_string_lossy(), &content[..end])
                .await?;
            return Err(FsError::Io("injected partial append failure".into()));
        }
        self.append_file(&path.to_string_lossy(), content).await?;
        self.append_bytes.fetch_add(content.len(), Ordering::AcqRel);
        if self.fail_after_append.swap(false, Ordering::AcqRel) {
            return Err(FsError::Io("injected failure after append".into()));
        }
        self.append_commits.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    async fn sync_file_rooted_no_follow(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<(), FsError> {
        let path = lingxi_core::host::rooted_fs::checked_join(root, relative)?;
        self.file_size(&path.to_string_lossy()).await.map(|_| ())
    }

    async fn truncate_file_rooted_no_follow(
        &self,
        root: &Path,
        relative: &Path,
        len: u64,
    ) -> Result<(), FsError> {
        let path = lingxi_core::host::rooted_fs::checked_join(root, relative)?;
        if self.fail_truncate.load(Ordering::Acquire) {
            return Err(FsError::Io("injected truncate failure".into()));
        }
        if len == 0 && self.fail_journal_clear.swap(false, Ordering::AcqRel) {
            return Err(FsError::Io("injected journal rotation failure".into()));
        }
        let mut files = self.files.lock().unwrap();
        let content = files
            .get_mut(path.to_string_lossy().as_ref())
            .ok_or_else(|| FsError::NotFound(path.to_string_lossy().into()))?;
        content.truncate(len as usize);
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
    fs: Arc<dyn FileSystem>,
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
        prompt: lingxi_core::types::utf16_json::Utf16JsonProjection::from(""),

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
        body: handback_frame("whole sanitized report").into(),

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
    let mut snapshot: serde_json::Value = serde_json::from_str(&file.content).unwrap();
    let journal_path = Path::new(&relative).with_extension("jsonl");
    let file = match fs.read_file_rooted_no_follow(root, &journal_path).await {
        Ok(file) => file,
        Err(FsError::NotFound(_)) => return snapshot,
        Err(error) => panic!("cannot read inbox journal: {error}"),
    };
    let sequence = snapshot["sequence"].as_u64().unwrap_or(0);
    for line in file.content.lines() {
        let record: serde_json::Value = serde_json::from_str(line).unwrap();
        if record["sequence"].as_u64().unwrap() <= sequence {
            continue;
        }
        let operation = &record["operation"];
        if let Some(admit) = operation.get("admit") {
            snapshot["pending"]
                .as_array_mut()
                .unwrap()
                .push(admit["envelope"].clone());
        } else if let Some(consume) = operation.get("consume") {
            snapshot["pending"]
                .as_array_mut()
                .unwrap()
                .retain(|envelope| envelope["receipt"] != consume["receipt"]);
            snapshot["consumed"]
                .as_array_mut()
                .unwrap()
                .push(consume["receipt"].clone());
        } else {
            panic!("unexpected inbox operation: {operation}");
        }
    }
    snapshot
}

fn inbox_path(
    root: &Path,
    scope: HandbackSessionScope,
    recipient: AgentId,
    extension: &str,
) -> String {
    root.join(format!(
        "{}-{}.handback-inbox.{extension}",
        scope.session_id.as_uuid().simple(),
        recipient.as_uuid().simple()
    ))
    .to_string_lossy()
    .into_owned()
}

fn peer_envelope(
    scope: HandbackSessionScope,
    recipient: AgentId,
    sender: AgentId,
    run_epoch: u64,
    message_id: MessageId,
) -> HandbackEnvelope {
    let mut report = report(sender);
    report.message_id = message_id;
    report.envelope(
        HandbackRunKey {
            scope,
            agent_id: sender,
            run_epoch,
        },
        HandbackRecipient::Agent {
            scope,
            agent_id: recipient,
        },
    )
}

async fn restore_actor(
    registry: &TaskRegistry,
    actor: AgentId,
    scope: HandbackSessionScope,
) -> Result<HandbackRunToken, crate::TaskError> {
    let mut input = start(actor, scope, None);
    input.restored_state = Some(HandbackState::new_run(
        HandbackRunKey {
            scope,
            agent_id: actor,
            run_epoch: 1,
        },
        true,
        None,
        None,
        HandbackRecipient::Main { scope },
        |_| true,
    ));
    registry.begin_handback_run(input).await
}

#[tokio::test]
async fn handback_legacy_snapshot_migrates_without_pruning_receipts_or_reordering_pending() {
    let root = tempfile::tempdir().unwrap();
    let fs = Arc::new(JournalFs::default());
    let original_scope = scope();
    let caller = AgentId::new();
    let sender = AgentId::new();
    let consumed = peer_envelope(original_scope, caller, sender, 1, MessageId::new()).receipt;
    let first = peer_envelope(original_scope, caller, sender, 2, MessageId::new());
    let second = peer_envelope(original_scope, caller, sender, 3, MessageId::new());
    fs.write_file(
        &inbox_path(root.path(), original_scope, caller, "json"),
        &serde_json::json!({
            "version": 1, "scope": original_scope, "recipient": caller,
            "pending": [first, second], "consumed": [consumed]
        })
        .to_string(),
    )
    .await
    .unwrap();
    let next_scope = HandbackSessionScope {
        activation_epoch: 2,
        ..original_scope
    };
    let (registry, _owner) = make_registry(root.path(), fs.clone(), next_scope);
    add_local(&registry, "caller", caller).await;
    restore_actor(&registry, caller, next_scope).await.unwrap();
    assert_eq!(
        registry.pending_handback_reports_for(caller).await,
        vec![first.clone(), second.clone()]
    );
    assert!(
        registry
            .acknowledge_handback_consumption(caller, &consumed)
            .await
    );
    assert_eq!(fs.append_commits.load(Ordering::Acquire), 0);
    assert!(
        registry
            .acknowledge_handback_consumption(caller, &first.receipt)
            .await
    );
    let third = peer_envelope(next_scope, caller, sender, 4, consumed.message_id);
    registry.send_peer("caller", third.clone()).await.unwrap();
    assert_eq!(
        registry.pending_handback_reports_for(caller).await,
        vec![second.clone(), third.clone()]
    );
    let (cold, _owner) = make_registry(root.path(), fs.clone(), next_scope);
    add_local(&cold, "caller", caller).await;
    restore_actor(&cold, caller, next_scope).await.unwrap();
    assert_eq!(
        cold.pending_handback_reports_for(caller).await,
        vec![second, third]
    );
    assert!(
        cold.acknowledge_handback_consumption(caller, &consumed)
            .await
    );
    assert!(
        cold.acknowledge_handback_consumption(caller, &first.receipt)
            .await
    );
    let persisted = journal(&fs, root.path(), next_scope, caller).await;
    assert_eq!(
        persisted["consumed"],
        serde_json::json!([consumed, first.receipt])
    );
}

#[tokio::test]
async fn handback_receipt_index_uses_every_identity_field_and_duplicate_writes_are_elided() {
    let root = tempfile::tempdir().unwrap();
    let fs = Arc::new(JournalFs::default());
    let scope = scope();
    let caller = AgentId::new();
    let sender = AgentId::new();
    let (registry, _owner) = make_registry(root.path(), fs.clone(), scope);
    add_local(&registry, "caller", caller).await;
    let envelope = peer_envelope(scope, caller, sender, 1, MessageId::new());
    registry
        .send_peer("caller", envelope.clone())
        .await
        .unwrap();
    registry
        .send_peer("caller", envelope.clone())
        .await
        .unwrap();
    assert!(
        registry
            .acknowledge_handback_consumption(caller, &envelope.receipt)
            .await
    );
    assert!(
        registry
            .acknowledge_handback_consumption(caller, &envelope.receipt)
            .await
    );
    registry
        .send_peer("caller", envelope.clone())
        .await
        .unwrap();
    assert_eq!(fs.append_commits.load(Ordering::Acquire), 2);
    assert!(registry
        .pending_handback_reports_for(caller)
        .await
        .is_empty());
    let mut wrong = envelope.receipt.clone();
    wrong.run.run_epoch += 1;
    assert!(
        !registry
            .acknowledge_handback_consumption(caller, &wrong)
            .await
    );
    wrong = envelope.receipt.clone();
    wrong.run.agent_id = AgentId::new();
    assert!(
        !registry
            .acknowledge_handback_consumption(caller, &wrong)
            .await
    );
    wrong = envelope.receipt.clone();
    wrong.run.scope.activation_epoch += 1;
    assert!(
        !registry
            .acknowledge_handback_consumption(caller, &wrong)
            .await
    );
    wrong = envelope.receipt.clone();
    wrong.recipient = HandbackRecipient::Agent {
        scope,
        agent_id: AgentId::new(),
    };
    assert!(
        !registry
            .acknowledge_handback_consumption(caller, &wrong)
            .await
    );
    for (sender, epoch) in [(sender, 2), (AgentId::new(), 1)] {
        let next = peer_envelope(scope, caller, sender, epoch, envelope.receipt.message_id);
        registry.send_peer("caller", next.clone()).await.unwrap();
        assert!(
            registry
                .acknowledge_handback_consumption(caller, &next.receipt)
                .await
        );
    }
    assert_eq!(fs.append_commits.load(Ordering::Acquire), 6);
}

#[tokio::test]
async fn handback_legacy_and_journal_replay_preserve_exact_unpaired_utf16_body_units() {
    use lingxi_core::types::utf16_json::Utf16JsonProjection;

    for legacy in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let fs = Arc::new(JournalFs::default());
        let scope = scope();
        let caller = AgentId::new();
        let sender = AgentId::new();
        let mut first = peer_envelope(scope, caller, sender, 1, MessageId::new());
        first.body = Utf16JsonProjection::parse(r#""A\ud800B\udfffC""#).unwrap();
        let exact_units = vec![
            u16::from(b'A'),
            0xD800,
            u16::from(b'B'),
            0xDFFF,
            u16::from(b'C'),
        ];
        if legacy {
            // Serializing the envelope directly preserves raw string leaves;
            // routing this fixture through Value would reject lone surrogates.
            let snapshot = format!(
                "{{\"version\":1,\"scope\":{},\"recipient\":{},\"pending\":[{}],\"consumed\":[]}}",
                serde_json::to_string(&scope).unwrap(),
                serde_json::to_string(&caller).unwrap(),
                serde_json::to_string(&first).unwrap(),
            );
            fs.write_file(&inbox_path(root.path(), scope, caller, "json"), &snapshot)
                .await
                .unwrap();
        } else {
            let (registry, _owner) = make_registry(root.path(), fs.clone(), scope);
            add_local(&registry, "caller", caller).await;
            registry.send_peer("caller", first.clone()).await.unwrap();
        }
        let (cold, _owner) = make_registry(root.path(), fs.clone(), scope);
        add_local(&cold, "caller", caller).await;
        restore_actor(&cold, caller, scope).await.unwrap();
        let pending = cold.pending_handback_reports_for(caller).await;
        assert_eq!(pending, vec![first.clone()]);
        assert_eq!(pending[0].body.string_units("").unwrap(), exact_units);
        assert!(
            cold.acknowledge_handback_consumption(caller, &first.receipt)
                .await
        );
        let mut second = peer_envelope(scope, caller, sender, 2, MessageId::new());
        second.body = first.body;
        cold.send_peer("caller", second.clone()).await.unwrap();
        let (replayed, _owner) = make_registry(root.path(), fs.clone(), scope);
        add_local(&replayed, "caller", caller).await;
        restore_actor(&replayed, caller, scope).await.unwrap();
        let pending = replayed.pending_handback_reports_for(caller).await;
        assert_eq!(pending, vec![second]);
        assert_eq!(pending[0].body.string_units("").unwrap(), exact_units);
        assert!(
            replayed
                .acknowledge_handback_consumption(caller, &first.receipt)
                .await
        );
    }
}

#[tokio::test]
async fn handback_append_failures_roll_back_partial_and_complete_records_before_retry() {
    for partial in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let fs = Arc::new(JournalFs::default());
        let scope = scope();
        let caller = AgentId::new();
        let sender = AgentId::new();
        let (registry, _owner) = make_registry(root.path(), fs.clone(), scope);
        add_local(&registry, "caller", caller).await;
        let envelope = peer_envelope(scope, caller, sender, 1, MessageId::new());
        if partial {
            fs.partial_append.store(true, Ordering::Release);
        } else {
            fs.fail_after_append.store(true, Ordering::Release);
        }
        assert!(registry
            .send_peer("caller", envelope.clone())
            .await
            .is_err());
        assert!(registry
            .pending_handback_reports_for(caller)
            .await
            .is_empty());
        let path = inbox_path(root.path(), scope, caller, "jsonl");
        assert_eq!(fs.file_size(&path).await.unwrap(), 0);
        registry
            .send_peer("caller", envelope.clone())
            .await
            .unwrap();
        let before = fs.file_size(&path).await.unwrap();
        if partial {
            fs.partial_append.store(true, Ordering::Release);
        } else {
            fs.fail_after_append.store(true, Ordering::Release);
        }
        assert!(
            !registry
                .acknowledge_handback_consumption(caller, &envelope.receipt)
                .await
        );
        assert_eq!(fs.file_size(&path).await.unwrap(), before);
        assert_eq!(
            registry.pending_handback_reports_for(caller).await,
            vec![envelope.clone()]
        );
        assert!(
            registry
                .acknowledge_handback_consumption(caller, &envelope.receipt)
                .await
        );
        let (cold, _owner) = make_registry(root.path(), fs.clone(), scope);
        add_local(&cold, "caller", caller).await;
        restore_actor(&cold, caller, scope).await.unwrap();
        assert!(cold.pending_handback_reports_for(caller).await.is_empty());
        assert!(
            cold.acknowledge_handback_consumption(caller, &envelope.receipt)
                .await
        );
    }
}

#[tokio::test]
async fn handback_replays_post_append_crash_and_duplicates_then_repairs_only_the_torn_tail() {
    let root = tempfile::tempdir().unwrap();
    let fs = Arc::new(JournalFs::default());
    let scope = scope();
    let caller = AgentId::new();
    let sender = AgentId::new();
    let (registry, _owner) = make_registry(root.path(), fs.clone(), scope);
    add_local(&registry, "caller", caller).await;
    let envelope = peer_envelope(scope, caller, sender, 1, MessageId::new());
    registry
        .send_peer("caller", envelope.clone())
        .await
        .unwrap();
    let path = inbox_path(root.path(), scope, caller, "jsonl");
    for (sequence, operation) in [
        (
            2,
            serde_json::json!({"consume": {"receipt": envelope.receipt}}),
        ),
        (
            3,
            serde_json::json!({"consume": {"receipt": envelope.receipt}}),
        ),
        (4, serde_json::json!({"admit": {"envelope": envelope}})),
    ] {
        fs.append_file(
            &path,
            &format!(
                "{}\n",
                serde_json::json!({
                    "version": 1, "sequence": sequence, "operation": operation
                })
            ),
        )
        .await
        .unwrap();
    }
    assert_eq!(
        registry.pending_handback_reports_for(caller).await,
        vec![envelope.clone()]
    );
    let committed_bytes = fs.file_size(&path).await.unwrap();
    fs.append_file(&path, "{\"version\":1,\"sequence\":5,\"operation\":")
        .await
        .unwrap();
    let (cold, _owner) = make_registry(root.path(), fs.clone(), scope);
    add_local(&cold, "caller", caller).await;
    restore_actor(&cold, caller, scope).await.unwrap();
    assert_eq!(fs.file_size(&path).await.unwrap(), committed_bytes);
    assert!(cold.pending_handback_reports_for(caller).await.is_empty());
    assert!(
        cold.acknowledge_handback_consumption(caller, &envelope.receipt)
            .await
    );
    cold.send_peer("caller", envelope.clone()).await.unwrap();
    assert_eq!(fs.file_size(&path).await.unwrap(), committed_bytes);
    fs.append_file(&path, "{corrupt complete record}\n")
        .await
        .unwrap();
    let corrupt_bytes = fs.file_size(&path).await.unwrap();
    let (corrupt, _owner) = make_registry(root.path(), fs.clone(), scope);
    add_local(&corrupt, "caller", caller).await;
    assert!(restore_actor(&corrupt, caller, scope).await.is_err());
    assert_eq!(fs.file_size(&path).await.unwrap(), corrupt_bytes);
}

#[tokio::test]
async fn handback_unconfirmed_append_blocks_fallback_and_recovers_without_duplicate_delivery() {
    let root = tempfile::tempdir().unwrap();
    let fs = Arc::new(JournalFs::default());
    let scope = scope();
    let caller = AgentId::new();
    let sender = AgentId::new();
    let (registry, main) = make_registry(root.path(), fs.clone(), scope);
    add_local(&registry, "caller", caller).await;
    let mut child = local("sender", sender, TaskStatus::Running);
    child.is_backgrounded = true;
    registry
        .insert_state_for_test(TaskState::LocalAgent(child))
        .await;
    let token = registry
        .begin_handback_run(start(sender, scope, Some(caller)))
        .await
        .unwrap();
    let report = report(sender);
    fs.fail_after_append.store(true, Ordering::Release);
    fs.fail_truncate.store(true, Ordering::Release);
    assert_eq!(
        registry.try_deliver_handback(&token, report.clone()).await,
        HandbackAdmissionOutcome::Rejected
    );
    let path = inbox_path(root.path(), scope, caller, "jsonl");
    let bytes = fs.file_size(&path).await.unwrap();
    assert_eq!(
        registry.try_deliver_handback(&token, report).await,
        HandbackAdmissionOutcome::Rejected
    );
    assert_eq!(fs.file_size(&path).await.unwrap(), bytes);
    assert!(main.accepted.lock().unwrap().is_empty());
    fs.fail_truncate.store(false, Ordering::Release);
    let (cold, _owner) = make_registry(root.path(), fs.clone(), scope);
    add_local(&cold, "caller", caller).await;
    restore_actor(&cold, caller, scope).await.unwrap();
    let pending = cold.pending_handback_reports_for(caller).await;
    assert_eq!(pending.len(), 1);
    cold.send_peer("caller", pending[0].clone()).await.unwrap();
    assert_eq!(cold.pending_handback_reports_for(caller).await, pending);
}

#[tokio::test]
async fn handback_reloaded_old_snapshot_owner_cannot_fallback_a_newer_wal_receipt() {
    let root = tempfile::tempdir().unwrap();
    let fs = Arc::new(JournalFs::default());
    let old_scope = scope();
    let caller = AgentId::new();
    let sender = AgentId::new();
    let (registry, main) = make_registry(root.path(), fs.clone(), old_scope);
    add_local(&registry, "caller", caller).await;
    let old = peer_envelope(old_scope, caller, AgentId::new(), 1, MessageId::new());
    registry.send_peer("caller", old.clone()).await.unwrap();
    assert!(
        registry
            .acknowledge_handback_consumption(caller, &old.receipt)
            .await
    );
    let scope = HandbackSessionScope {
        activation_epoch: 2,
        ..old_scope
    };
    *main.scope.lock().unwrap() = scope;
    restore_actor(&registry, caller, scope).await.unwrap();
    let mut sender_row = local("sender", sender, TaskStatus::Running);
    sender_row.is_backgrounded = true;
    registry
        .insert_state_for_test(TaskState::LocalAgent(sender_row.clone()))
        .await;
    let token = registry
        .begin_handback_run(start(sender, scope, Some(caller)))
        .await
        .unwrap();
    let report = report(sender);
    let receipt = admitted(registry.try_deliver_handback(&token, report.clone()).await);
    let commits = fs.append_commits.load(Ordering::Acquire);
    let (cold, cold_main) = make_registry(root.path(), fs.clone(), scope);
    add_local(&cold, "caller", caller).await;
    cold.insert_state_for_test(TaskState::LocalAgent(sender_row))
        .await;
    let cold_token = cold
        .begin_handback_run(start(sender, scope, Some(caller)))
        .await
        .unwrap();
    assert_eq!(cold_token.run(), token.run());
    assert_eq!(
        cold.try_deliver_handback(&cold_token, report.clone()).await,
        HandbackAdmissionOutcome::Rejected
    );
    assert!(cold_main.accepted.lock().unwrap().is_empty());
    assert_eq!(fs.append_commits.load(Ordering::Acquire), commits);
    restore_actor(&cold, caller, scope).await.unwrap();
    assert_eq!(
        admitted(cold.try_deliver_handback(&cold_token, report).await),
        receipt
    );
    assert_eq!(cold.pending_handback_reports_for(caller).await.len(), 1);
    assert_eq!(fs.append_commits.load(Ordering::Acquire), commits);
}

#[tokio::test]
async fn handback_long_history_compaction_has_amortized_io_and_replays_a_stale_journal_prefix() {
    const ROUNDS: u64 = 768;
    let root = tempfile::tempdir().unwrap();
    let fs = Arc::new(JournalFs::default());
    let scope = scope();
    let caller = AgentId::new();
    let sender = AgentId::new();
    let (registry, _owner) = make_registry(root.path(), fs.clone(), scope);
    add_local(&registry, "caller", caller).await;
    let mut receipts = Vec::new();
    for epoch in 1..=ROUNDS {
        let envelope = peer_envelope(scope, caller, sender, epoch, MessageId::new());
        registry
            .send_peer("caller", envelope.clone())
            .await
            .unwrap();
        assert!(
            registry
                .acknowledge_handback_consumption(caller, &envelope.receipt)
                .await
        );
        receipts.push(envelope.receipt);
    }
    assert_eq!(
        fs.append_commits.load(Ordering::Acquire),
        ROUNDS as usize * 2
    );
    assert!(fs.atomic_commits.load(Ordering::Acquire) < ROUNDS as usize / 8);
    assert!(fs.atomic_bytes.load(Ordering::Acquire) <= fs.append_bytes.load(Ordering::Acquire) * 3);
    assert_eq!(
        fs.reads.load(Ordering::Acquire),
        3,
        "hot operations must not reread history"
    );
    // Simulate a crash after a new snapshot is published, before its old
    // journal is cleared; then append another report to that stale prefix.
    fs.fail_journal_clear.store(true, Ordering::Release);
    let snapshot_path = inbox_path(root.path(), scope, caller, "json");
    let before_sequence = {
        let files = fs.files.lock().unwrap();
        serde_json::from_str::<serde_json::Value>(&files[&snapshot_path]).unwrap()["sequence"]
            .as_u64()
            .unwrap()
    };
    let mut epoch = ROUNDS;
    loop {
        epoch += 1;
        assert!(
            epoch < ROUNDS * 4,
            "compaction should eventually cross its byte threshold"
        );
        let envelope = peer_envelope(scope, caller, sender, epoch, MessageId::new());
        registry
            .send_peer("caller", envelope.clone())
            .await
            .unwrap();
        assert!(
            registry
                .acknowledge_handback_consumption(caller, &envelope.receipt)
                .await
        );
        receipts.push(envelope.receipt);
        let files = fs.files.lock().unwrap();
        let snapshot: serde_json::Value = serde_json::from_str(&files[&snapshot_path]).unwrap();
        if snapshot["sequence"].as_u64().unwrap() > before_sequence {
            break;
        }
    }
    assert!(!fs.fail_journal_clear.load(Ordering::Acquire));
    let pending = peer_envelope(scope, caller, sender, epoch + 1, MessageId::new());
    registry.send_peer("caller", pending.clone()).await.unwrap();
    let (cold, _owner) = make_registry(root.path(), fs.clone(), scope);
    add_local(&cold, "caller", caller).await;
    restore_actor(&cold, caller, scope).await.unwrap();
    assert_eq!(
        cold.pending_handback_reports_for(caller).await,
        vec![pending]
    );
    let before = fs.append_commits.load(Ordering::Acquire);
    for receipt in receipts {
        assert!(
            cold.acknowledge_handback_consumption(caller, &receipt)
                .await
        );
    }
    assert_eq!(fs.append_commits.load(Ordering::Acquire), before);
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
    fs.fail_append.store(true, Ordering::Release);
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
    assert_eq!(fs.atomic_commits.load(Ordering::Acquire), 1);
    assert_eq!(fs.append_commits.load(Ordering::Acquire), 2);
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
    fs.block_append.store(true, Ordering::Release);
    let ack = {
        let registry = registry.clone();
        let receipt = receipt.clone();
        tokio::spawn(async move {
            registry
                .acknowledge_handback_consumption(caller, &receipt)
                .await
        })
    };
    tokio::time::timeout(DEADLINE, fs.append_entered.notified())
        .await
        .expect("ACK must reach the blocked journal commit");
    assert_eq!(registry.pending_handback_reports_for(caller).await.len(), 1);
    ack.abort();
    assert!(ack.await.unwrap_err().is_cancelled());
    fs.append_release.notify_one();
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
    assert_eq!(fs.atomic_commits.load(Ordering::Acquire), 1);
    assert_eq!(fs.append_commits.load(Ordering::Acquire), 2);
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
    async fn send(
        &self,
        _: lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> Result<(), TaskRegistryError> {
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

#[tokio::test]
async fn handback_delivered_report_does_not_enqueue_a_second_completion_query() {
    use lingxi_core::host::task_delivery::TaskDeliveryPolicy;
    let root = tempfile::tempdir().unwrap();
    let scope = scope();
    let (registry, main) = make_registry(root.path(), Arc::new(JournalFs::default()), scope);
    let actor = AgentId::new();
    add_local(&registry, "reported", actor).await;
    let token = registry
        .begin_handback_run(start(actor, scope, None))
        .await
        .unwrap();
    let receipt = admitted(registry.try_deliver_handback(&token, report(actor)).await);
    registry
        .mark_task_rested_with_finalizing(
            "reported",
            Some(handback_pointer(false, "worker").into()),
            None,
            Some(actor),
            None,
            None,
            Some(token.run()),
            true,
        )
        .await;
    assert!(
        !registry.has_pending_task_notifications_for(None).await,
        "an admitted report owns delivery; completion cannot wake the same parent again"
    );
    assert!(registry
        .take_pending_task_notifications(TaskDeliveryPolicy::default())
        .await
        .is_empty());
    assert_eq!(main.accepted.lock().unwrap().len(), 1);
    assert_eq!(
        registry.handback_state(&token).await.unwrap().receipt,
        Some(receipt)
    );
    let state = registry.get("reported").await.unwrap();
    assert!(state.base().notified);
    assert!(state.is_parked(), "the agent must remain resumable");
}

#[tokio::test]
async fn handback_completion_without_report_and_post_report_failures_still_notify() {
    use lingxi_core::host::task_delivery::TaskDeliveryPolicy;
    use lingxi_core::host::task_registry::AgentTerminalOutcome;
    for kind in ["no_report", "error", "worktree", "turn_limit"] {
        let root = tempfile::tempdir().unwrap();
        let scope = scope();
        let (registry, _main) = make_registry(root.path(), Arc::new(JournalFs::default()), scope);
        let actor = AgentId::new();
        add_local(&registry, "sender", actor).await;
        let token = registry
            .begin_handback_run(start(actor, scope, None))
            .await
            .unwrap();
        if kind != "no_report" {
            admitted(registry.try_deliver_handback(&token, report(actor)).await);
        }
        registry
            .set_agent_outcome(
                "sender",
                AgentTerminalOutcome {
                    error: (kind == "error").then(|| "failure after report".into()),
                    worktree_path: (kind == "worktree").then(|| "/new/worktree".into()),
                    max_turns_reached: (kind == "turn_limit").then_some(3),
                    ..Default::default()
                },
            )
            .await;
        registry
            .mark_task_rested(
                "sender",
                Some("completion".into()),
                None,
                Some(actor),
                None,
                None,
                Some(token.run()),
            )
            .await;
        assert!(
            registry.has_pending_task_notifications_for(None).await,
            "{kind} has new information"
        );
        assert_eq!(
            registry
                .take_pending_task_notifications(TaskDeliveryPolicy::default())
                .await
                .len(),
            1,
            "{kind}"
        );
    }
}

#[tokio::test]
async fn handback_reported_completion_retains_owned_work_until_it_is_consumed() {
    let root = tempfile::tempdir().unwrap();
    let scope = scope();
    let (registry, _main) = make_registry(root.path(), Arc::new(JournalFs::default()), scope);
    let actor = AgentId::new();
    add_local(&registry, "sender", actor).await;
    let token = registry
        .begin_handback_run(start(actor, scope, None))
        .await
        .unwrap();
    admitted(registry.try_deliver_handback(&token, report(actor)).await);
    let mut child = local("owned", AgentId::new(), TaskStatus::Running);
    child.base.creator_agent_id = Some(actor);
    registry
        .insert_state_for_test(TaskState::LocalAgent(child))
        .await;
    registry
        .mark_task_rested(
            "sender",
            Some("pointer".into()),
            None,
            Some(actor),
            None,
            None,
            Some(token.run()),
        )
        .await;
    assert!(!registry.has_pending_task_notifications_for(None).await);
    assert!(!registry.get("sender").await.unwrap().base().notified);
    registry
        .set_status("owned", TaskStatus::Completed)
        .await
        .unwrap();
    registry.mark_notified("owned").await.unwrap();
    assert!(!registry.has_pending_task_notifications_for(None).await);
    assert!(registry.get("sender").await.unwrap().base().notified);
}

#[tokio::test]
async fn handback_completion_retargets_when_the_report_recipient_is_gone() {
    use lingxi_core::host::task_delivery::TaskDeliveryPolicy;
    let root = tempfile::tempdir().unwrap();
    let scope = scope();
    let (registry, _main) = make_registry(root.path(), Arc::new(JournalFs::default()), scope);
    let caller = AgentId::new();
    let actor = AgentId::new();
    add_local(&registry, "caller", caller).await;
    add_local(&registry, "sender", actor).await;
    let token = registry
        .begin_handback_run(start(actor, scope, Some(caller)))
        .await
        .unwrap();
    admitted(registry.try_deliver_handback(&token, report(actor)).await);
    registry
        .mark_task_rested(
            "sender",
            Some("completed".into()),
            None,
            Some(actor),
            None,
            None,
            Some(token.run()),
        )
        .await;
    registry
        .set_status("caller", TaskStatus::Killed)
        .await
        .unwrap();
    let notes = registry
        .take_pending_task_notifications(TaskDeliveryPolicy::default())
        .await;
    assert!(
        notes.iter().any(|note| note.task_id == "sender"),
        "the new fallback recipient still needs the completion"
    );
}

#[tokio::test]
async fn handback_in_turn_notification_drain_deduplicates_without_an_idle_poll() {
    use lingxi_core::host::task_delivery::TaskDeliveryPolicy;
    let root = tempfile::tempdir().unwrap();
    let scope = scope();
    let (registry, main) = make_registry(root.path(), Arc::new(JournalFs::default()), scope);
    let actor = AgentId::new();
    add_local(&registry, "sender", actor).await;
    let token = registry
        .begin_handback_run(start(actor, scope, None))
        .await
        .unwrap();
    admitted(registry.try_deliver_handback(&token, report(actor)).await);
    registry
        .mark_task_rested(
            "sender",
            Some("report pointer".into()),
            None,
            Some(actor),
            None,
            None,
            Some(token.run()),
        )
        .await;
    assert!(registry
        .take_pending_task_notifications(TaskDeliveryPolicy::default())
        .await
        .is_empty());
    assert_eq!(main.accepted.lock().unwrap().len(), 1);
    assert!(registry.get("sender").await.unwrap().is_parked());
}
