use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use lingxi_core::host::OrchestratorHandle;
use serde_json::json;
use tool_api::read_file_state::{get, new_read_file_state_map, ReadFileStateMap};
use tool_api::test_support::{ctx_for_file_tools, fresh_ctx, fresh_tx, make_dummy_fs};
use tool_api::tool_trait::{generated_builtin_result, Tool, ToolCallResult, ToolError};
use tool_api::BuiltinToolContext;
use tool_file::{FileEditTool, FileReadTool, FileWriteTool};

struct Fixture {
    _temp: tempfile::TempDir,
    cwd: PathBuf,
    home: PathBuf,
    session_id: Uuid,
    rows: Vec<Value>,
    tools: BuiltinToolContext,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("project");
        std::fs::create_dir(&cwd).unwrap();
        let home = temp.path().join("home");
        let tools = ctx_for_file_tools(
            make_dummy_fs(),
            Arc::new(telemetry::AnalyticsBus::new()),
            vec![cwd.clone()],
        );
        Self {
            _temp: temp,
            cwd,
            home,
            session_id: Uuid::new_v4(),
            rows: Vec::new(),
            tools,
        }
    }

    fn context_with_map(&self, map: ReadFileStateMap) -> BuiltinToolContext {
        let mut context = self.tools.clone();
        context.read_file_state = map;
        context
    }

    async fn record(&mut self, name: &str, input: Value) {
        let tool: Box<dyn Tool> = match name {
            "Write" => Box::new(FileWriteTool::new(self.tools.clone())),
            "Read" => Box::new(FileReadTool::new(self.tools.clone())),
            "Edit" => Box::new(FileEditTool::new(self.tools.clone())),
            _ => panic!("unsupported fixture tool"),
        };
        let result = tool
            .call(generated_builtin_result(&input), fresh_ctx(), fresh_tx())
            .await
            .unwrap();
        assert!(!result.is_error);
        let use_id = format!("call-{}", self.rows.len());
        let assistant_id = Uuid::new_v4();
        let parent = self.rows.last().map(|row| row["uuid"].clone());
        self.rows.push(json!({
            "type": "assistant", "uuid": assistant_id, "parentUuid": parent,
            "sessionId": self.session_id, "cwd": self.cwd,
            "timestamp": "2026-10-19T07:00:00.000Z", "isSidechain": false,
            "message": {"role": "assistant", "model": "deepseek-flash", "content": [
                {"type": "tool_use", "id": use_id, "name": name, "input": input}
            ]}
        }));
        let raw: Value = serde_json::from_str(&result.data.to_json_string().unwrap()).unwrap();
        let model_text = result
            .model_content
            .as_ref()
            .and_then(|content| content.string_at(""))
            .unwrap_or("Tool completed");
        self.rows.push(json!({
            "type": "user", "uuid": Uuid::new_v4(), "parentUuid": assistant_id,
            "sessionId": self.session_id, "cwd": self.cwd,
            "timestamp": "2026-10-19T07:00:01.000Z", "isSidechain": false,
            "toolUseResult": raw,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": use_id,
                 "content": model_text, "is_error": false}
            ]}
        }));
    }

    fn persist(&self) {
        let path = session::jsonl::session_path(
            &self.home,
            &self.cwd.to_string_lossy(),
            &self.session_id.to_string(),
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let runtime = crate::test_support::code_resume_runtime_snapshot(SessionId::from_uuid(
            self.session_id,
        ));
        let content = runtime
            .transcript_metadata
            .into_iter()
            .flatten()
            .map(|row| format!("{}\n", row.to_json_string().unwrap()))
            .chain(self.rows.iter().map(|row| format!("{row}\n")))
            .collect::<String>();
        std::fs::write(path, content).unwrap();
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        Arc::new(platform_posix::fs::PosixFileSystem::new(
            self._temp.path().to_path_buf(),
        ))
    }

    fn fresh_orchestrator(&self, map: ReadFileStateMap) -> ConversationOrchestrator {
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::with_files(vec![])),
            self.cwd.clone(),
        )
        .with_read_state_map(map)
    }

    async fn cold_resume(&self, map: ReadFileStateMap) -> ConversationOrchestrator {
        self.persist();
        ConversationOrchestrator::with_resume(
            OrchestratorConfig::default(),
            self.session_id,
            self.home.clone(),
            self.cwd.to_string_lossy().into_owned(),
            self.fs(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::with_files(vec![])),
            self.cwd.clone(),
            None,
        )
        .await
        .unwrap()
        .with_read_state_map(map)
    }

    async fn hot_resume(&self, orchestrator: &ConversationOrchestrator) {
        self.persist();
        let replayed = replay_session_state(
            &self.home,
            &self.cwd.to_string_lossy(),
            self.session_id,
            self.fs(),
        )
        .await
        .unwrap();
        let runtime = replayed.handle_runtime_snapshot();
        orchestrator
            .resume_session(
                replayed.state.session_id,
                replayed.state.history,
                replayed.last_message_uuid.map(|id| id.to_string()),
                None,
                runtime,
            )
            .await
            .unwrap();
    }
}

async fn edit(context: &BuiltinToolContext, path: &Path) -> Result<ToolCallResult, ToolError> {
    FileEditTool::new(context.clone())
        .call(
            generated_builtin_result(&json!({
                "file_path": path, "old_string": "before", "new_string": "after"
            })),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
}

fn assert_stale(result: Result<ToolCallResult, ToolError>) {
    let ToolError::InvalidInput(error) = result.unwrap_err() else {
        panic!("expected stale-file refusal");
    };
    assert_eq!(
        error.string_at(""),
        Some(tool_file::FILE_UNEXPECTEDLY_MODIFIED_ERROR)
    );
}

#[tokio::test]
async fn cold_file_state_resume_write_then_edit_uses_shared_map() {
    let mut fixture = Fixture::new();
    let path = fixture.cwd.join("created.html");
    fixture
        .record("Write", json!({"file_path": path, "content": "before\n"}))
        .await;
    let map = new_read_file_state_map();
    let _orchestrator = fixture.cold_resume(map.clone()).await;
    assert_eq!(get(&map, &path).unwrap().content, "before\n");
    edit(&fixture.context_with_map(map), &path).await.unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "after\n");
}

#[tokio::test]
async fn hot_file_state_resume_restores_latest_edit_and_isolates_session() {
    let mut fixture = Fixture::new();
    let path = fixture.cwd.join("edited.js");
    fixture
        .record("Write", json!({"file_path": path, "content": "first\n"}))
        .await;
    fixture
        .record(
            "Edit",
            json!({"file_path": path, "old_string": "first", "new_string": "before"}),
        )
        .await;
    let map = new_read_file_state_map();
    let context = fixture.context_with_map(map.clone());
    let old_path = fixture.cwd.join("other-session.txt");
    FileWriteTool::new(context.clone())
        .call(
            generated_builtin_result(
                &json!({"file_path": old_path, "content": "private old session"}),
            ),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .unwrap();
    let orchestrator = fixture.fresh_orchestrator(map.clone());
    fixture.hot_resume(&orchestrator).await;
    assert!(get(&map, &old_path).is_none());
    assert_eq!(get(&map, &path).unwrap().content, "before\n");
    edit(&context, &path).await.unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "after\n");
}

#[tokio::test]
async fn cli_file_state_resume_restores_full_read_without_write() {
    let mut fixture = Fixture::new();
    let path = fixture.cwd.join("read.txt");
    std::fs::write(&path, "before\r\n").unwrap();
    fixture.record("Read", json!({"file_path": path})).await;
    let map = new_read_file_state_map();
    let orchestrator = fixture.fresh_orchestrator(map.clone());
    let rows = fixture
        .rows
        .iter()
        .map(|row| serde_json::from_value(row.clone()).unwrap())
        .collect::<Vec<_>>();
    *orchestrator.session.lock().await = state_from_messages(fixture.session_id, &rows);
    orchestrator.restore_resume_runtime_metadata(&rows).await;
    assert_eq!(get(&map, &path).unwrap().content, "before\n");
    edit(&fixture.context_with_map(map), &path).await.unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"after\r\n");
}

#[tokio::test]
async fn native_file_state_resume_replays_mixed_and_cr_only_edit_sidecars() {
    for (before, expected_view, expected_disk) in [
        ("before\r\ntail\n", "after\ntail\n", "after\ntail\n"),
        ("before\rtail\r", "after\rtail\r", "after\rtail\r"),
        (
            "before\r\ntail\r\nlast\n",
            "after\ntail\nlast\n",
            "after\r\ntail\r\nlast\r\n",
        ),
    ] {
        let mut fixture = Fixture::new();
        let path = fixture.cwd.join("mixed.txt");
        std::fs::write(&path, before).unwrap();
        fixture.record("Read", json!({"file_path": path})).await;
        fixture
            .record(
                "Edit",
                json!({"file_path":path,"old_string":"before","new_string":"after"}),
            )
            .await;
        let map = new_read_file_state_map();
        let orchestrator = fixture.fresh_orchestrator(map.clone());
        let rows = fixture
            .rows
            .iter()
            .map(|row| serde_json::from_value(row.clone()).unwrap())
            .collect::<Vec<_>>();
        *orchestrator.session.lock().await = state_from_messages(fixture.session_id, &rows);
        orchestrator.restore_resume_runtime_metadata(&rows).await;
        assert_eq!(get(&map, &path).unwrap().content, expected_view);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected_disk);
    }
}

#[tokio::test]
async fn file_state_resume_rejects_external_changes_even_with_preserved_or_older_mtime() {
    for hot in [false, true] {
        for older in [false, true] {
            let mut fixture = Fixture::new();
            let path = fixture.cwd.join("changed.txt");
            fixture
                .record("Write", json!({"file_path": path, "content": "before\n"}))
                .await;
            let original_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
            // Keep the edit's old_string applicable: stale protection, rather
            // than a string-mismatch error, must protect the external line.
            let external = "before\nexternal work\n";
            std::fs::write(&path, external).unwrap();
            let mtime = if older {
                original_mtime - std::time::Duration::from_secs(60)
            } else {
                original_mtime
            };
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(mtime))
                .unwrap();
            let map = new_read_file_state_map();
            let orchestrator = if hot {
                let orchestrator = fixture.fresh_orchestrator(map.clone());
                fixture.hot_resume(&orchestrator).await;
                orchestrator
            } else {
                fixture.cold_resume(map.clone()).await
            };
            let context = fixture.context_with_map(map);
            assert_stale(edit(&context, &path).await);
            assert_stale(
                FileWriteTool::new(context.clone())
                    .call(
                        generated_builtin_result(
                            &json!({"file_path": path, "content": "overwrite"}),
                        ),
                        fresh_ctx(),
                        fresh_tx(),
                    )
                    .await,
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), external);
            FileReadTool::new(context.clone())
                .call(
                    generated_builtin_result(&json!({"file_path": path})),
                    fresh_ctx(),
                    fresh_tx(),
                )
                .await
                .unwrap();
            edit(&context, &path).await.unwrap();
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "after\nexternal work\n"
            );
            drop(orchestrator);
        }
    }
}

#[tokio::test]
async fn replay_of_a_real_changed_file_notice_does_not_revive_its_old_write_baseline() {
    let mut fixture = Fixture::new();
    let path = fixture.cwd.join("changed.txt");
    fixture
        .record("Write", json!({"file_path":path,"content":"before\n"}))
        .await;
    let map = fixture.tools.read_file_state.clone();
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(FileReadTool::new(fixture.tools.clone())));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![
            crate::test_support::mock_message_response(
                vec![llm_runtime::ContentBlock::Text {
                    text: "file change observed".into(),
                    cache_control: None,
                    citations: None,
                }],
                Some("end_turn"),
            ),
        ])),
        Arc::new(tools),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        fixture.cwd.clone(),
    )
    .with_read_state_map(map.clone())
    .with_jsonl_writer(Arc::new(session::JsonlWriter::new(
        session::jsonl::session_path(
            &fixture.home,
            &fixture.cwd.to_string_lossy(),
            &fixture.session_id.to_string(),
        ),
        fixture.fs(),
    )));
    // Resume owns the import of existing Native transcript identities. Attach
    // the writer before admission so subsequent appends exercise real storage.
    fixture.hot_resume(&orch).await;
    std::fs::write(&path, "after\n").unwrap();
    orch.run_turn("observe the current file").await.unwrap();
    assert_eq!(
        orch.transcript
            .model_reminder_attachments
            .lock()
            .unwrap()
            .values()
            .filter(|source| source.string_at("/type") == Some("edited_text_file"))
            .count(),
        1
    );
    let replay = replay_session_state(
        &fixture.home,
        &fixture.cwd.to_string_lossy(),
        fixture.session_id,
        fixture.fs(),
    )
    .await
    .unwrap();
    let runtime = replay.handle_runtime_snapshot();
    let proof = replay
        .state
        .history
        .iter()
        .map(|message| {
            let source = runtime
                .model_reminder_attachments
                .iter()
                .find(|(id, _)| *id == message.id())
                .map(|(_, source)| source);
            (
                message.id(),
                source
                    .and_then(|source| source.string_at("/type"))
                    .map(str::to_owned),
                source
                    .and_then(|source| source.string_at("/filename"))
                    .map(str::to_owned),
                replay
                    .state
                    .model_context_excluded_messages
                    .contains(&message.id()),
            )
        })
        .collect::<Vec<_>>();
    assert!(
        proof
            .iter()
            .any(|(_, kind, _, _)| kind.as_deref() == Some("edited_text_file")),
        "the real published reminder must survive in the active JSONL chain: {proof:?}"
    );
    orch.resume_session(
        replay.state.session_id,
        replay.state.history,
        replay.last_message_uuid.map(|id| id.to_string()),
        None,
        runtime,
    )
    .await
    .unwrap();
    assert!(
        get(&map, &path).is_none(),
        "a diff cannot restore the superseded complete A snapshot: {:?}",
        get(&map, &path)
    );
    assert!(
        orch.changed_files_reminder_messages(None).await.is_empty(),
        "the published A-to-B change must not be replayed as a new notice"
    );
}
