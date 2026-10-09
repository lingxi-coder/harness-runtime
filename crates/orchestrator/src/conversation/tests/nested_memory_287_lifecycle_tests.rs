//! Current nested-memory dedup survives real JSONL and lifecycle transitions.

use super::*;
use crate::resume::{replay_session_state, ReplayedSession};
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use lingxi_core::host::{FileSystem, OrchestratorHandle};
use platform_posix::fs::PosixFileSystem;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tool_api::Tool;

struct Fixture {
    _temp: tempfile::TempDir,
    cwd: PathBuf,
    home: PathBuf,
    trigger: PathBuf,
    memory: PathBuf,
    session_id: SessionId,
    jsonl_path: PathBuf,
    fs: Arc<dyn FileSystem>,
}

impl Fixture {
    fn new(body: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let cwd = root.join("repo");
        let home = root.join("home");
        let trigger = cwd.join("pkg/api/handler.rs");
        let memory = cwd.join("pkg").join(branding::MEMORY_FILE);
        write(&trigger, "fn handler() {}\n");
        write(&memory, body);
        std::fs::create_dir_all(&home).unwrap();
        let session_id = SessionId::new();
        let jsonl_path = session::jsonl::path::session_path(
            &home,
            &cwd.to_string_lossy(),
            &session_id.as_uuid().to_string(),
        );
        std::fs::create_dir_all(jsonl_path.parent().unwrap()).unwrap();
        let fs = Arc::new(PosixFileSystem::new(root)) as Arc<dyn FileSystem>;
        Self {
            _temp: temp,
            cwd,
            home,
            trigger,
            memory,
            session_id,
            jsonl_path,
            fs,
        }
    }

    fn writer(&self) -> Arc<JsonlWriter> {
        Arc::new(JsonlWriter::new(self.jsonl_path.clone(), self.fs.clone()))
    }

    fn root(&self) -> Arc<ConversationOrchestrator> {
        let root = Arc::new(
            ConversationOrchestrator::new(
                OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(Vec::new())),
                Arc::new(ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                self.cwd.clone(),
            )
            .with_session_id(self.session_id)
            .with_nested_memory_roots(self.home.clone(), None)
            .with_config_home(self.home.clone())
            .with_jsonl_writer(self.writer()),
        );
        root.attach_owned_session_switches();
        root
    }

    async fn replay(&self) -> ReplayedSession {
        replay_session_state(
            &self.home,
            &self.cwd.to_string_lossy(),
            self.session_id.as_uuid(),
            self.fs.clone(),
        )
        .await
        .unwrap()
    }

    async fn cold(&self) -> Arc<ConversationOrchestrator> {
        let root = Arc::new(
            ConversationOrchestrator::with_resume(
                OrchestratorConfig::default(),
                self.session_id.as_uuid(),
                self.home.clone(),
                self.cwd.to_string_lossy().into_owned(),
                self.fs.clone(),
                Arc::new(MockApiClient::new(Vec::new())),
                Arc::new(ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                self.cwd.clone(),
                Some(self.writer()),
            )
            .await
            .unwrap()
            .with_nested_memory_roots(self.home.clone(), None)
            .with_config_home(self.home.clone()),
        );
        root.attach_owned_session_switches();
        root
    }

    async fn hot(&self, root: &ConversationOrchestrator) {
        let replayed = self.replay().await;
        let runtime = replayed.handle_runtime_snapshot();
        root.resume_session(
            self.session_id,
            replayed.state.history,
            replayed.last_message_uuid.map(|id| id.to_string()),
            None,
            runtime,
        )
        .await
        .unwrap();
    }

    async fn rows(&self) -> Vec<session::JsonlMessage> {
        tokio::fs::read_to_string(&self.jsonl_path)
            .await
            .unwrap()
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// Drive discovery from the production Read tool and its shared registry.
async fn read_trigger(root: &ConversationOrchestrator, fixture: &Fixture) {
    let mut ctx = tool_api::test_support::ctx_for_file_tools(
        fixture.fs.clone(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![fixture.cwd.clone()],
    );
    ctx.read_file_state = root.prompt_runtime.read_state_map.clone();
    tool_file::FileReadTool::new(ctx)
        .call(
            json!({"file_path": fixture.trigger}),
            {
                let mut call = tool_api::test_support::fresh_ctx();
                call.nested_memory_triggers = root.prompt_runtime.nested_memory_triggers.clone();
                call
            },
            tool_api::test_support::fresh_tx(),
        )
        .await
        .unwrap();
    let entry =
        tool_api::read_file_state::get(&root.prompt_runtime.read_state_map, &fixture.trigger)
            .expect("successful Read supplies the touched-file input");
    assert!(entry.from_read);
    assert!(!entry.seeded_from_context);
}

/// Raw attachments precede a real resumable user tip in the persisted chain.
async fn persist_user(root: &ConversationOrchestrator, text: &str) -> ConversationMessage {
    let message = ConversationMessage::user(MessageId::new(), text.into());
    root.session.lock().await.history.push(message.clone());
    root.persist_message_to_jsonl(&message).await;
    message
}

async fn initial_memory(
    root: &ConversationOrchestrator,
    fixture: &Fixture,
) -> (Vec<ConversationMessage>, ConversationMessage) {
    persist_user(root, "inspect the handler").await;
    read_trigger(root, fixture).await;
    let messages = root.nested_memory_reminder_messages().await;
    assert!(!messages.is_empty());
    let tail = persist_user(root, "continue after reading the handler").await;
    (messages, tail)
}

fn nested_rows(rows: &[session::JsonlMessage]) -> Vec<&session::JsonlMessage> {
    rows.iter()
        .filter(|row| {
            row.message_type == "attachment"
                && row
                    .extra
                    .get("attachment")
                    .is_some_and(|value| value["type"] == "nested_memory")
        })
        .collect()
}

fn attachment_for<'a>(rows: &'a [session::JsonlMessage], path: &Path) -> &'a Value {
    &nested_rows(rows)
        .into_iter()
        .find(|row| row.extra["attachment"]["path"] == json!(path))
        .expect("one persisted attachment per acquired file")
        .extra["attachment"]
}

fn assert_restored_file_row<'a>(
    rows: &'a [session::JsonlMessage],
    fixture: &Fixture,
    body: &str,
) -> &'a session::JsonlMessage {
    let restored = rows
        .iter()
        .filter(|row| {
            row.message_type == "attachment"
                && row.extra["attachment"]["type"] == "file"
                && row.extra["attachment"]["filename"] == json!(fixture.memory)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        restored.len(),
        1,
        "one durable file row owns both API text blocks"
    );
    let row = restored[0];
    assert!(row.message.is_null());
    let attachment = &row.extra["attachment"];
    assert_eq!(attachment["content"]["type"], "text");
    let file = &attachment["content"]["file"];
    assert_eq!(file["filePath"], json!(fixture.memory));
    assert_eq!(file["content"], body);
    assert_eq!(file["startLine"], 1);
    assert_eq!(file["numLines"], body.split('\n').count());
    assert_eq!(file["totalLines"], body.split('\n').count());
    assert_eq!(
        attachment["displayPath"],
        format!("pkg/{}", branding::MEMORY_FILE)
    );
    row
}

fn assert_linear_active_parent_chain(rows: &[session::JsonlMessage]) {
    for pair in rows.windows(2) {
        assert_eq!(pair[1].parent_uuid.as_deref(), Some(pair[0].uuid.as_str()));
    }
}

fn assert_restored_read_state(root: &ConversationOrchestrator, path: &Path, body: &str) {
    let entry = tool_api::read_file_state::get(&root.prompt_runtime.read_state_map, path)
        .expect("native compact restoration re-reads the folder instruction file");
    assert_eq!(entry.content, body);
    assert!(entry.from_read);
    assert!(!entry.seeded_from_context);
    assert!(!entry.is_partial_view);
    // The SDK's full-read offset None represents native Read's startLine 1.
    assert!(entry.offset.is_none());
    assert!(entry.limit.is_none());
}

fn assert_seeded(root: &ConversationOrchestrator, path: &Path, raw: &str) {
    let entry = tool_api::read_file_state::get(&root.prompt_runtime.read_state_map, path)
        .expect("unchanged history still seeds the acquired file");
    assert_eq!(entry.content, raw);
    assert!(entry.seeded_from_context);
    assert!(!entry.from_read);
    assert!(entry.offset.is_none());
    assert!(entry.limit.is_none());
}

#[tokio::test]
async fn filesystem_imports_persist_per_file_bytes_and_dedup_after_cold_and_hot_resume() {
    let parent_body = " \nparent guidance\n@imported.md\n\t";
    let child_body = "\r\n imported guidance \t\n";
    let oracle: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/nested_memory_2_1_287.json"
    ))
    .unwrap();
    let native = oracle["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "native-acquired-import-crlf-body-keeps-native-disk-content")
        .unwrap();
    assert_eq!(native["input"]["files"][0]["content"], parent_body);
    assert_eq!(native["input"]["files"][1]["content"], child_body);
    let child_seed = native["expected"]["readState"][1]["entry"]["content"]
        .as_str()
        .unwrap();
    let fixture = Fixture::new(parent_body);
    let imported = fixture.memory.parent().unwrap().join("imported.md");
    write(&imported, child_body);
    let root = fixture.root();
    let (messages, _) = initial_memory(&root, &fixture).await;
    assert_eq!(messages.len(), 2, "imports receive separate durable rows");
    let rows = fixture.rows().await;
    let raw = nested_rows(&rows);
    assert_eq!(raw.len(), 2);
    for (row, message) in raw.iter().zip(&messages) {
        assert_eq!(row.uuid, message.id().as_uuid().to_string());
        assert!(row.message.is_null());
        let attachment = &row.extra["attachment"];
        let file = &attachment["content"];
        assert_eq!(attachment["path"], file["path"]);
        assert_eq!(file["type"], "Project");
        assert_eq!(file["contentDiffersFromDisk"], false);
        assert!(file.get("rawContent").is_none());
        assert_eq!(
            message.text_content(),
            format!(
                "<system-reminder>\nContents of {}:\n\n{}\n</system-reminder>",
                file["path"].as_str().unwrap(),
                file["content"].as_str().unwrap()
            )
        );
    }
    let parent = attachment_for(&rows, &fixture.memory);
    assert_eq!(parent["content"]["content"], parent_body);
    assert!(parent["content"].get("parent").is_none());
    assert_eq!(
        parent["displayPath"],
        format!("pkg/{}", branding::MEMORY_FILE)
    );
    let child = attachment_for(&rows, &imported);
    assert_eq!(child["content"]["content"], child_body);
    assert_eq!(child["content"]["parent"], json!(fixture.memory));
    assert_eq!(child["displayPath"], "pkg/imported.md");

    let cold = fixture.cold().await;
    assert!(
        tool_api::read_file_state::get(&cold.prompt_runtime.read_state_map, &fixture.memory)
            .is_none()
    );
    read_trigger(&cold, &fixture).await;
    assert!(cold.nested_memory_reminder_messages().await.is_empty());
    assert_seeded(&cold, &fixture.memory, parent_body);
    assert_seeded(&cold, &imported, child_seed);
    let prior = cold.nested_memory_history().await;
    assert_eq!(prior.nested[&imported], "imported guidance");

    fixture.hot(&root).await;
    assert!(
        tool_api::read_file_state::get(&root.prompt_runtime.read_state_map, &fixture.memory)
            .is_none()
    );
    read_trigger(&root, &fixture).await;
    assert!(root.nested_memory_reminder_messages().await.is_empty());
    assert_seeded(&root, &fixture.memory, parent_body);
    assert_seeded(&root, &imported, child_seed);
    let sidecars = root.transcript.model_reminder_attachments.lock().unwrap();
    for row in raw {
        let id = MessageId::from_uuid(uuid::Uuid::parse_str(&row.uuid).unwrap());
        assert_eq!(sidecars[&id], row.extra["attachment"]);
        assert_eq!(
            serde_json::to_vec(&sidecars[&id]).unwrap(),
            serde_json::to_vec(&row.extra["attachment"]).unwrap()
        );
    }
    drop(sidecars);
    assert_eq!(
        nested_rows(&fixture.rows().await).len(),
        2,
        "both resumes suppress a duplicate persistent attachment"
    );
}

#[tokio::test]
async fn changed_body_after_cold_or_hot_resume_emits_new_raw_body_once() {
    for cold_resume in [false, true] {
        let fixture = Fixture::new(" \noriginal guidance\n\t");
        let root = fixture.root();
        let (original, _) = initial_memory(&root, &fixture).await;
        let changed = "\n changed guidance \t\n";
        write(&fixture.memory, changed);
        let resumed = if cold_resume {
            fixture.cold().await
        } else {
            fixture.hot(&root).await;
            root.clone()
        };
        read_trigger(&resumed, &fixture).await;
        let emitted = resumed.nested_memory_reminder_messages().await;
        assert_eq!(emitted.len(), 1, "cold={cold_resume}");
        assert_ne!(emitted[0].id(), original[0].id());
        assert_eq!(
            emitted[0].text_content(),
            format!(
                "<system-reminder>\nContents of {}:\n\n{changed}\n</system-reminder>",
                fixture.memory.display()
            )
        );
        assert!(resumed.nested_memory_reminder_messages().await.is_empty());
        assert_seeded(&resumed, &fixture.memory, changed);
        persist_user(&resumed, "continue with the changed instructions").await;
        let replayed = fixture.replay().await;
        let rows = nested_rows(&replayed.messages);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].uuid, emitted[0].id().as_uuid().to_string());
        assert!(rows[1].message.is_null());
        assert_eq!(rows[1].extra["attachment"]["content"]["content"], changed);
        assert_eq!(
            resumed.nested_memory_history().await.nested[&fixture.memory],
            "changed guidance"
        );
    }
}

async fn compact(root: &ConversationOrchestrator, messages_to_preserve: Vec<ConversationMessage>) {
    let result = compaction::IterationCompactionResult {
        messages: vec![ConversationMessage::compact_summary(
            MessageId::new(),
            "controlled compact summary".into(),
        )],
        messages_to_preserve,
        media_analysis_to_preserve: Vec::new(),
        raw_summary_text: "controlled compact summary".into(),
        layers_applied: vec![compaction::CompactionLayer::Autocompact],
        total_tokens_freed: 1,
        cache_hit: false,
        consecutive_failures: 0,
        was_compacted: true,
        rapid_refill_breaker_tripped: false,
        consecutive_rapid_refills: 0,
        compaction_usage: None,
        compaction_model: None,
        compaction_profile: None,
    };
    assert!(root
        .apply_post_compact(
            result,
            compaction::CompactTrigger::Manual,
            10_000,
            3,
            40_000,
            std::time::Instant::now(),
            None
        )
        .await
        .is_some());
}

#[tokio::test]
async fn retained_raw_nested_memory_survives_real_compaction_and_cold_or_hot_resume() {
    for cold_resume in [false, true] {
        let body = " \nretained guidance\n\t";
        let fixture = Fixture::new(body);
        let root = fixture.root();
        let (mut kept, tail) = initial_memory(&root, &fixture).await;
        let original = kept[0].clone();
        let original_rows = fixture.rows().await;
        let original_payload = attachment_for(&original_rows, &fixture.memory).clone();
        kept.push(tail);
        compact(&root, kept).await;
        assert_restored_read_state(&root, &fixture.memory, body);
        assert_eq!(
            root.nested_memory_history().await.nested[&fixture.memory],
            "retained guidance"
        );
        read_trigger(&root, &fixture).await;
        assert!(
            root.nested_memory_reminder_messages().await.is_empty(),
            "the retained raw body suppresses reattachment immediately after compaction"
        );
        assert_restored_read_state(&root, &fixture.memory, body);
        let replayed = fixture.replay().await;
        assert_linear_active_parent_chain(&replayed.messages);
        let restored_file = assert_restored_file_row(&replayed.messages, &fixture, body);
        let restored_id = MessageId::from_uuid(uuid::Uuid::parse_str(&restored_file.uuid).unwrap());
        let restored_payload = restored_file.extra["attachment"].clone();
        let raw = nested_rows(&replayed.messages);
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0].uuid, original.id().as_uuid().to_string());
        assert_eq!(raw[0].extra["attachment"], original_payload);
        let resumed = if cold_resume {
            fixture.cold().await
        } else {
            fixture.hot(&root).await;
            root.clone()
        };
        {
            assert_eq!(
                resumed
                    .transcript
                    .model_reminder_attachments
                    .lock()
                    .unwrap()[&restored_id],
                restored_payload
            );
            let message = resumed
                .session
                .lock()
                .await
                .history
                .iter()
                .find(|message| message.id() == restored_id)
                .cloned()
                .unwrap();
            assert_eq!(
                message,
                ConversationOrchestrator::file_attachment_projection(
                    restored_id,
                    &restored_payload
                )
                .unwrap()
            );
        }
        assert!(tool_api::read_file_state::get(
            &resumed.prompt_runtime.read_state_map,
            &fixture.memory
        )
        .is_none());
        read_trigger(&resumed, &fixture).await;
        assert!(
            resumed.nested_memory_reminder_messages().await.is_empty(),
            "cold={cold_resume}: retained body must dedup without a restored path cursor"
        );
        assert_seeded(&resumed, &fixture.memory, body);
        assert_eq!(
            nested_rows(&fixture.rows().await).len(),
            1,
            "compaction and resume keep the original row bytes"
        );
    }
}

#[tokio::test]
async fn discarded_raw_nested_memory_does_not_suppress_after_compaction_and_cold_or_hot_resume() {
    for cold_resume in [false, true] {
        let body = " \ndiscarded guidance\n\t";
        let fixture = Fixture::new(body);
        let root = fixture.root();
        let (original, _) = initial_memory(&root, &fixture).await;
        compact(&root, Vec::new()).await;
        assert_restored_read_state(&root, &fixture.memory, body);
        assert!(root.nested_memory_history().await.nested.is_empty());
        assert_eq!(
            nested_rows(&fixture.rows().await).len(),
            1,
            "the old raw row still physically exists"
        );
        let replayed_after_compact = fixture.replay().await;
        assert_linear_active_parent_chain(&replayed_after_compact.messages);
        let restored = assert_restored_file_row(&replayed_after_compact.messages, &fixture, body);
        let restored_id = MessageId::from_uuid(uuid::Uuid::parse_str(&restored.uuid).unwrap());
        let restored_payload = restored.extra["attachment"].clone();
        assert!(
            nested_rows(&replayed_after_compact.messages).is_empty(),
            "the resumable chain excludes the discarded row"
        );
        let resumed = if cold_resume {
            fixture.cold().await
        } else {
            fixture.hot(&root).await;
            root.clone()
        };
        assert_eq!(
            resumed
                .transcript
                .model_reminder_attachments
                .lock()
                .unwrap()[&restored_id],
            restored_payload
        );
        assert!(tool_api::read_file_state::get(
            &resumed.prompt_runtime.read_state_map,
            &fixture.memory
        )
        .is_none());
        // Archived sidecars alone must never revive a discarded baseline.
        resumed
            .restore_resume_prompt_metadata(&fixture.rows().await)
            .await;
        assert!(resumed.nested_memory_history().await.nested.is_empty());
        read_trigger(&resumed, &fixture).await;
        let emitted = resumed.nested_memory_reminder_messages().await;
        assert_eq!(
            emitted.len(),
            1,
            "cold={cold_resume}: body absent after the compact boundary must attach again"
        );
        assert_ne!(emitted[0].id(), original[0].id());
        assert_seeded(&resumed, &fixture.memory, body);
        persist_user(&resumed, "continue after discarded context reload").await;
        let replayed = fixture.replay().await;
        let raw = nested_rows(&replayed.messages);
        assert_eq!(raw.len(), 1);
        assert_eq!(raw[0].uuid, emitted[0].id().as_uuid().to_string());
        assert!(raw[0].message.is_null());
        assert_eq!(raw[0].extra["attachment"]["content"]["content"], body);
        assert_eq!(nested_rows(&fixture.rows().await).len(), 2);
    }
}

#[tokio::test]
async fn discarded_body_restores_as_file_after_real_compaction_without_resume() {
    let body = " \ndiscarded guidance\n\t";
    let fixture = Fixture::new(body);
    let root = fixture.root();
    let (original, _) = initial_memory(&root, &fixture).await;
    compact(&root, Vec::new()).await;
    assert!(root.nested_memory_history().await.nested.is_empty());
    read_trigger(&root, &fixture).await;
    assert!(root.nested_memory_reminder_messages().await.is_empty(),
        "the full compact ReadState snapshot restores a file instead of attaching nested memory twice");
    assert_restored_read_state(&root, &fixture.memory, body);
    assert!(root.nested_memory_reminder_messages().await.is_empty());
    persist_user(&root, "continue with restored file instructions").await;
    let replayed = fixture.replay().await;
    let raw = nested_rows(&replayed.messages);
    assert!(raw.is_empty());
    assert_linear_active_parent_chain(&replayed.messages);
    let restored = assert_restored_file_row(&replayed.messages, &fixture, body);
    assert_ne!(restored.uuid, original[0].id().as_uuid().to_string());
    assert_eq!(nested_rows(&fixture.rows().await).len(), 1);
}

#[tokio::test]
async fn later_memory_creation_requires_a_new_read_trigger() {
    let mut fixture = Fixture::new("new guidance");
    std::fs::remove_file(&fixture.memory).unwrap();
    let root = fixture.root();
    read_trigger(&root, &fixture).await;
    assert!(root.nested_memory_reminder_messages().await.is_empty());
    assert!(root
        .prompt_runtime
        .nested_memory_triggers
        .queued()
        .is_empty());
    write(&fixture.memory, "new guidance");
    assert!(
        root.nested_memory_reminder_messages().await.is_empty(),
        "cached touched paths must not re-scan later files"
    );
    fixture.trigger = fixture.cwd.join("pkg/api/second.rs");
    write(&fixture.trigger, "fn second() {}\n");
    read_trigger(&root, &fixture).await;
    let messages = root.nested_memory_reminder_messages().await;
    assert_eq!(messages.len(), 1);
    assert!(messages[0].text_content().contains("new guidance"));
}

#[tokio::test]
async fn trigger_survives_cache_eviction_before_attachment_acquisition() {
    let fixture = Fixture::new("guidance after eviction");
    let root = fixture.root();
    read_trigger(&root, &fixture).await;
    {
        let mut cache = root.prompt_runtime.read_state_map.lock().unwrap();
        for index in 0..tool_api::read_file_state::READ_FILE_STATE_MAX_ENTRIES {
            cache.set(
                fixture.cwd.join(format!("cached-{index}.rs")),
                tool_api::read_file_state::ReadFileEntry {
                    content: "cache-only write".into(),
                    mtime_ms: 0,
                    offset: None,
                    limit: None,
                    from_read: false,
                    seeded_from_context: false,
                    is_partial_view: false,
                },
            );
        }
        assert!(!cache.contains(&fixture.trigger));
    }
    assert_eq!(
        root.prompt_runtime.nested_memory_triggers.queued(),
        vec![fixture.trigger.clone()]
    );
    let messages = root.nested_memory_reminder_messages().await;
    assert_eq!(messages.len(), 1);
    assert!(messages[0]
        .text_content()
        .contains("guidance after eviction"));
}

#[tokio::test]
async fn a_cached_model_visible_entry_is_not_a_read_trigger() {
    let fixture = Fixture::new("not implicitly discovered");
    let root = fixture.root();
    tool_api::read_file_state::set(
        &root.prompt_runtime.read_state_map,
        fixture.trigger.clone(),
        tool_api::read_file_state::ReadFileEntry {
            content: "cached content".into(),
            mtime_ms: 0,
            offset: None,
            limit: None,
            from_read: true,
            seeded_from_context: false,
            is_partial_view: false,
        },
    );
    assert!(root.nested_memory_reminder_messages().await.is_empty());
    assert_eq!(
        root.prompt_runtime
            .read_state_map
            .lock()
            .unwrap()
            .model_context_keys(),
        vec![fixture.trigger]
    );
}

#[tokio::test]
async fn failed_and_unchanged_reads_do_not_enqueue_nested_memory() {
    let fixture = Fixture::new("initial guidance");
    let root = fixture.root();
    read_trigger(&root, &fixture).await;
    assert_eq!(root.nested_memory_reminder_messages().await.len(), 1);
    read_trigger(&root, &fixture).await;
    assert!(
        root.prompt_runtime
            .nested_memory_triggers
            .queued()
            .is_empty(),
        "ordinary file_unchanged returns before the native enqueue site"
    );
    let mut builtin = tool_api::test_support::ctx_for_file_tools(
        fixture.fs.clone(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![fixture.cwd.clone()],
    );
    builtin.read_file_state = root.prompt_runtime.read_state_map.clone();
    let mut context = tool_api::test_support::fresh_ctx();
    context.nested_memory_triggers = root.prompt_runtime.nested_memory_triggers.clone();
    let missing = fixture.cwd.join("pkg/api/missing.rs");
    assert!(tool_file::FileReadTool::new(builtin)
        .call(
            json!({"file_path":missing}),
            context,
            tool_api::test_support::fresh_tx()
        )
        .await
        .is_err());
    assert!(root
        .prompt_runtime
        .nested_memory_triggers
        .queued()
        .is_empty());
}

#[tokio::test]
async fn child_read_context_does_not_enqueue_the_main_context() {
    let fixture = Fixture::new("child-only read");
    let root = fixture.root();
    let mut builtin = tool_api::test_support::ctx_for_file_tools(
        fixture.fs.clone(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![fixture.cwd.clone()],
    );
    builtin.read_file_state = root.prompt_runtime.read_state_map.clone();
    let mut child = tool_api::test_support::fresh_ctx();
    child.agent_id = Some(lingxi_core::types::AgentId::new());
    let queue = child.nested_memory_triggers.clone();
    tool_file::FileReadTool::new(builtin)
        .call(
            json!({"file_path":fixture.trigger}),
            child,
            tool_api::test_support::fresh_tx(),
        )
        .await
        .unwrap();
    assert_eq!(queue.queued(), vec![fixture.trigger.clone()]);
    assert!(root.nested_memory_reminder_messages().await.is_empty());
    assert!(
        tool_api::read_file_state::get(&root.prompt_runtime.read_state_map, &fixture.trigger)
            .is_some()
    );
}
