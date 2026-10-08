use super::*;
use lingxi_core::host::instructions::InstructionContextProvider as _;
use memory::lingxi_md::LingxiMdTier;
use serde_json::{json, Value};
use std::path::PathBuf;

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

#[test]
fn agents_eager_modes_order_and_dedup_match_executed_286_hooks() {
    let oracle: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/instructions-2.1.286/agents.json"
    ))
    .unwrap();
    for case in oracle["cases"].as_array().unwrap() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let path = |text: &str| root.join(text.strip_prefix("/fixture/").unwrap());
        let cwd = root.join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        for file in case["engine"].as_array().unwrap() {
            write(
                &path(file["path"].as_str().unwrap()),
                file["content"].as_str().unwrap(),
            );
        }
        for group in case["groups"].as_array().unwrap() {
            for file in group["parts"].as_array().unwrap() {
                write(
                    &path(file["path"].as_str().unwrap()),
                    file["content"].as_str().unwrap(),
                );
            }
        }
        // Keep upstream oracle bytes unchanged; adapt its option vocabulary
        // only at the test import boundary.
        let options = match case["options"]["instructionFiles"].as_str() {
            Some("claude-md") => json!({"instructionFiles":"lingxi-md"}),
            Some("claude-md-and-agents-md") => {
                json!({"instructionFiles":"lingxi-md-and-agents-md"})
            }
            _ if case["options"]["projectInstructions"] == "both" => {
                json!({"instructionFiles":"lingxi-md-and-agents-md"})
            }
            _ => case["options"].clone(),
        };
        let mode = InstructionFilesMode::from_options(&options);
        let home = root.join("home");
        let files = load_memory_files_at_with_user_config_dir(
            &cwd,
            &home,
            &home.join(branding::DOT_DIR),
            &root.join("managed"),
            None,
            mode,
            true,
        );
        let actual: Vec<Value> = files.into_iter().map(|file| json!({
            "path":format!("/fixture/{}",file.path.strip_prefix(&root).unwrap().display()),
            "content":file.body,
            "kind":match file.tier { LingxiMdTier::Managed=>"managed",LingxiMdTier::User=>"user",LingxiMdTier::Project=>"project",LingxiMdTier::Local=>"local" },
        })).collect();
        let expected: Vec<Value> = case["expected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| json!({"path":file["path"],"content":file["content"],"kind":file["kind"]}))
            .collect();
        assert_eq!(actual, expected, "{}", case["name"]);
    }
}

#[derive(Clone)]
struct FixtureProvider {
    home: PathBuf,
    managed: PathBuf,
    mode: InstructionFilesMode,
}

#[async_trait]
impl MemoryHierarchyProvider for FixtureProvider {
    async fn load(&self, cwd: &Path) -> Vec<MemoryFile> {
        load_memory_files_at_with_user_config_dir(
            cwd,
            &self.home,
            &self.home.join(branding::DOT_DIR),
            &self.managed,
            None,
            self.mode,
            true,
        )
    }
    async fn load_with_mode(&self, cwd: &Path, mode: InstructionFilesMode) -> Vec<MemoryFile> {
        load_memory_files_at_with_user_config_dir(
            cwd,
            &self.home,
            &self.home.join(branding::DOT_DIR),
            &self.managed,
            None,
            mode,
            true,
        )
    }
    async fn load_conditional_rules(
        &self,
        cwd: &Path,
        trigger: &Path,
        mode: InstructionFilesMode,
    ) -> Vec<MemoryFile> {
        super::super::nested_memory::discover_conditional_rules(
            trigger,
            cwd,
            &self.home,
            Some(&self.managed),
            None,
            mode,
        )
    }
    fn instruction_files_mode(&self) -> InstructionFilesMode {
        self.mode
    }
    fn filesystem_discovery(&self) -> bool {
        true
    }
    fn hierarchy_roots(&self) -> Option<(PathBuf, Option<PathBuf>)> {
        Some((self.home.clone(), Some(self.managed.clone())))
    }
}

fn root_provider(
    memory: Arc<dyn MemoryHierarchyProvider>,
    cwd: &Path,
) -> (
    Arc<RootInstructionContextProvider>,
    Arc<crate::ConversationOrchestrator>,
) {
    use crate::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    };
    let root = Arc::new(crate::ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        memory,
        cwd.to_owned(),
    ));
    let provider = Arc::new(RootInstructionContextProvider::new());
    provider.bind(&root).unwrap();
    (provider, root)
}

#[tokio::test]
async fn actual_main_request_uses_agents_with_telemetry_disabled_and_frozen_fork_context() {
    use crate::test_support::{
        noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    };
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let cwd = root.join("repo");
    write(&cwd.join("AGENTS.md"), "AGENTS_ACTUAL_REQUEST");
    let provider = Arc::new(FixtureProvider {
        home: root.join("home"),
        managed: root.join("managed"),
        mode: InstructionFilesMode::default(),
    });
    let api = Arc::new(MockApiClient::new(vec![
        crate::test_support::mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "ok".into(),
                cache_control: None, citations: None,
            }],
            Some("end_turn"),
        ),
    ]));
    let orch = crate::ConversationOrchestrator::new(
        crate::OrchestratorConfig {
            model: "gpt-5.4".into(),
            ..Default::default()
        },
        api.clone(),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        provider,
        cwd.clone(),
    );
    orch.run_turn("hello").await.unwrap();
    let sent = api.captured_msgs().await;
    assert!(serde_json::to_string(&sent[0])
        .unwrap()
        .contains("AGENTS_ACTUAL_REQUEST"));
    write(&cwd.join("AGENTS.md"), "CHANGED_AFTER_PARENT_DISPATCH");
    let snapshot = orch.instruction_context_snapshot().await;
    assert!(snapshot.user_context["instructions"].contains("AGENTS_ACTUAL_REQUEST"));
    assert!(!snapshot.user_context["instructions"].contains("CHANGED_AFTER_PARENT_DISPATCH"));
}

#[tokio::test]
async fn nested_agents_full_partial_self_reads_and_fork_cursor() {
    use lingxi_core::host::instructions::InstructionScope;
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let cwd = root.join("repo");
    let nested = cwd.join("pkg/AGENTS.md");
    write(&nested, "NESTED_INSTRUCTIONS");
    write(&cwd.join("pkg/main.rs"), "fn main() {}");
    let (provider, _owner) = root_provider(
        Arc::new(FixtureProvider {
            home: root.join("home"),
            managed: root.join("managed"),
            mode: InstructionFilesMode::default(),
        }),
        &cwd,
    );
    let mut context = provider.load(&cwd, InstructionScope::Full).await.unwrap();
    assert!(provider
        .after_read(&cwd, &nested, true, &mut context)
        .await
        .agents_context
        .is_empty());
    assert!(!context.sent_paths.contains(&nested));
    let attached = provider
        .after_read(&cwd, &cwd.join("pkg/main.rs"), false, &mut context)
        .await;
    assert_eq!(attached.agents_context.len(), 1);
    assert_eq!(
        attached.agents_context[0],
        format!("Contents of {}:\n\nNESTED_INSTRUCTIONS", nested.display())
    );
    assert!(attached.legacy_reminders.is_empty());
    let mut fork = context.clone();
    assert!(provider
        .after_read(&cwd, &cwd.join("pkg/main.rs"), false, &mut fork)
        .await
        .agents_context
        .is_empty());
    let mut full = provider.load(&cwd, InstructionScope::Full).await.unwrap();
    assert!(provider
        .after_read(&cwd, &nested, false, &mut full)
        .await
        .agents_context
        .is_empty());
    assert!(full.sent_paths.contains(&nested));
    assert!(provider
        .after_read(&cwd, &cwd.join("pkg/main.rs"), false, &mut full)
        .await
        .agents_context
        .is_empty());
}

#[tokio::test]
async fn managed_only_retains_policy_and_empty_injection_never_discovers_disk() {
    use lingxi_core::host::instructions::InstructionScope;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let cwd = root.join("repo");
    write(&cwd.join("AGENTS.md"), "PROJECT");
    write(&root.join("managed/LINGXI.md"), "POLICY");
    let (provider, _owner) = root_provider(
        Arc::new(FixtureProvider {
            home: root.join("home"),
            managed: root.join("managed"),
            mode: InstructionFilesMode::default(),
        }),
        &cwd,
    );
    let managed = provider
        .load(&cwd, InstructionScope::ManagedOnly)
        .await
        .unwrap();
    assert!(managed.user_context["instructions"].contains("POLICY"));
    assert!(!managed.user_context["instructions"].contains("PROJECT"));
    let (empty, _empty_owner) = root_provider(
        Arc::new(crate::test_support::StaticMemoryProvider::empty()),
        &cwd,
    );
    let mut context = empty.load(&cwd, InstructionScope::Full).await.unwrap();
    assert_eq!(context.eager_instructions, Some(Vec::new()));
    assert_eq!(context.user_context["instructions"], "");
    assert_eq!(
        context.user_context["currentDate"],
        format!(
            "Today's date is {}.",
            crate::prompt::env_meta::current_date_string()
        )
    );
    assert_eq!(
        context.user_context.len(),
        2,
        "known empty instructions retain the native context anchor and date"
    );
    let delivered = empty
        .after_read(&cwd, &cwd.join("child/read.txt"), false, &mut context)
        .await;
    assert!(delivered.agents_context.is_empty());
    assert!(delivered.legacy_reminders.is_empty());
}

#[tokio::test]
async fn child_read_context_separates_legacy_reminders_from_agents_hook_frames() {
    use lingxi_core::host::instructions::InstructionScope;
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let cwd = root.join("repo");
    let legacy = cwd.join("pkg/LINGXI.md");
    let agents = cwd.join("pkg/AGENTS.md");
    write(&legacy, "LEGACY_INSTRUCTION");
    write(&agents, "AGENTS_INSTRUCTION");
    let (provider, _owner) = root_provider(
        Arc::new(FixtureProvider {
            home: root.join("home"),
            managed: root.join("managed"),
            mode: InstructionFilesMode::LingxiMdAndAgentsMd,
        }),
        &cwd,
    );
    let mut context = provider.load(&cwd, InstructionScope::Full).await.unwrap();
    let delivered = provider
        .after_read(&cwd, &cwd.join("pkg/main.rs"), false, &mut context)
        .await;
    assert_eq!(
        delivered.legacy_reminders,
        vec![format!(
            "<system-reminder>\nContents of {}:\n\nLEGACY_INSTRUCTION\n</system-reminder>",
            legacy.display()
        )]
    );
    assert_eq!(
        delivered.agents_context,
        vec![format!(
            "Contents of {}:\n\nAGENTS_INSTRUCTION",
            agents.display()
        )]
    );
    assert!(context.sent_paths.contains(&legacy));
    assert!(context.sent_paths.contains(&agents));
    let repeated = provider
        .after_read(&cwd, &cwd.join("pkg/main.rs"), false, &mut context)
        .await;
    assert!(repeated.legacy_reminders.is_empty());
    assert!(repeated.agents_context.is_empty());
}

/// Successful Read result fixtures deliberately do not populate readFileState:
/// the plugin must observe Read outcomes, including media and dedup results.
struct SuccessfulReadFixture;

#[async_trait]
impl tool_api::tool_trait::Tool for SuccessfulReadFixture {
    fn name(&self) -> &str {
        "Read"
    }
    fn input_schema(&self) -> &Value {
        static SCHEMA: once_cell::sync::Lazy<Value> = once_cell::sync::Lazy::new(|| {
            json!({
                "type":"object",
                "properties": {
                    "file_path":{"type":"string"},
                    "offset":{"type":"integer"},
                    "limit":{"type":"integer"},
                    "pages":{"type":"string"},
                    "test_error":{"type":"boolean"},
                    "test_truncated":{"type":"boolean"},
                    "test_unchanged":{"type":"boolean"}
                },
                "required":["file_path"]
            })
        });
        &SCHEMA
    }
    fn is_enabled(&self, _: &tool_api::tool_trait::ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024 * 1024
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    async fn check_permissions(
        &self,
        _: &Value,
        _: &tool_api::context::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "Read outcome fixture".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }
    async fn description(&self, _: &Value, _: &tool_api::tool_trait::DescriptionOptions) -> String {
        "Read outcome fixture".into()
    }
    async fn prompt(&self, _: &tool_api::tool_trait::PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        input: Value,
        _: tool_api::context::ToolUseContext,
        _: tool_api::progress::ToolProgressSender,
    ) -> Result<tool_api::tool_trait::ToolCallResult, tool_api::tool_trait::ToolError> {
        let path = input["file_path"].as_str().unwrap();
        let data = if input["test_unchanged"] == true {
            json!({"type":"file_unchanged", "filePath":path, "content":"unchanged"})
        } else if path.ends_with(".png") {
            json!({"type":"image", "file":{"base64":"aW1hZ2U=", "type":"image/png", "originalSize":5}})
        } else if path.ends_with(".pdf") {
            json!({"type":"pdf", "file":{"filePath":path,"base64":"JVBERi0=","originalSize":5}})
        } else {
            json!({"type":"text", "content":"read body", "file":{"truncatedByTokenCap":input["test_truncated"] == true}})
        };
        let mut result = tool_api::tool_trait::ToolCallResult::from_data(data);
        result.is_error = input["test_error"] == true;
        Ok(result)
    }
}

fn read_response(id: &str, input: Value) -> llm_runtime::HistoryResponse {
    crate::test_support::mock_message_response(
        vec![llm_runtime::ContentBlock::ToolCall { input_projection: None,
            id: id.to_owned(),
            name: "Read".into(),
            input,
        }],
        Some("tool_use"),
    )
}

fn finished_response() -> llm_runtime::HistoryResponse {
    crate::test_support::mock_message_response(
        vec![llm_runtime::ContentBlock::Text {
            text: "done".into(),
            cache_control: None, citations: None,
        }],
        Some("end_turn"),
    )
}

fn read_fixture_orchestrator(
    api: Arc<crate::test_support::MockApiClient>,
    root: &Path,
) -> crate::ConversationOrchestrator {
    use crate::test_support::{noop_hook_executor, MockOutputStream, NoOpPermissionGate};
    let mut tools = tool_api::registry::ToolRegistry::new();
    tools.register_builtin(Arc::new(SuccessfulReadFixture));
    crate::ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        api,
        Arc::new(tools),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(FixtureProvider {
            home: root.to_path_buf(),
            managed: root.join("managed"),
            mode: InstructionFilesMode::LingxiMdAndAgentsMd,
        }),
        root.join("repo"),
    )
}

#[tokio::test]
async fn successful_read_agents_context_survives_requests_and_cold_replay() {
    use crate::test_support::MockApiClient;
    for (path, options) in [
        ("pkg/main.rs", json!({})),
        ("~/repo/pkg/main.rs", json!({})),
        ("pkg/main.rs", json!({"offset":2,"limit":1})),
        ("pkg/snapshot.png", json!({})),
        ("pkg/report.pdf", json!({"pages":"2"})),
        ("pkg/main.rs", json!({"test_unchanged":true})),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let agents = root.join("repo/pkg/AGENTS.md");
        write(&agents, "KEEP_NESTED_INSTRUCTION");
        let mut input = options;
        input["file_path"] = json!(path);
        let api = Arc::new(MockApiClient::new(vec![
            read_response("read-first", input.clone()),
            read_response("read-again", input),
            finished_response(),
            finished_response(),
        ]));
        let transcript = root.join("session.jsonl");
        let fs: Arc<dyn lingxi_core::host::FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(root.clone()));
        let writer = Arc::new(session::JsonlWriter::new(transcript.clone(), fs));
        let orch = read_fixture_orchestrator(api.clone(), &root).with_jsonl_writer(writer);
        orch.run_turn("read").await.unwrap();
        orch.run_turn("continue").await.unwrap();
        let expected = format!(
            "<system-reminder>\ntool.call hook additional context: Contents of {}:\n\nKEEP_NESTED_INSTRUCTION\n</system-reminder>",
            agents.display()
        );
        let calls = api.captured_msgs().await;
        let contexts = |messages: &[lingxi_core::types::ConversationMessage]| {
            messages
                .iter()
                .filter(|message| message.text_content() == expected)
                .cloned()
                .collect::<Vec<_>>()
        };
        assert!(contexts(&calls[0]).is_empty(), "{path}");
        let original = contexts(&calls[1]);
        assert_eq!(original.len(), 1, "{path}");
        assert_eq!(contexts(&calls[2]), original, "{path}");
        assert_eq!(contexts(&calls[3]), original, "{path}");

        let rows: Vec<session::JsonlMessage> = std::fs::read_to_string(&transcript)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let attachments: Vec<_> = rows
            .iter()
            .filter_map(|row| row.extra.get("attachment"))
            .filter(|attachment| attachment["hookName"] == "tool.call")
            .collect();
        let expected_attachment = hooks::additional_context_attachment(
            "tool.call",
            "read-first-context",
            "PostToolUse",
            &[hooks::ExactHookText::from_text(format!(
                "Contents of {}:\n\nKEEP_NESTED_INSTRUCTION",
                agents.display()
            ))],
        );
        assert_eq!(attachments, vec![&expected_attachment.value], "{path}");
        let resumed_api = Arc::new(MockApiClient::new(vec![finished_response()]));
        let resumed = read_fixture_orchestrator(resumed_api.clone(), &root);
        let session_id = orch.session.lock().await.session_id;
        *resumed.session.lock().await =
            crate::resume::state_from_messages(session_id.as_uuid(), &rows);
        resumed.restore_resume_runtime_metadata(&rows).await;
        resumed.run_turn("resume").await.unwrap();
        assert_eq!(
            contexts(&resumed_api.captured_msgs().await[0]).len(),
            1,
            "{path}"
        );
    }
}

#[tokio::test]
async fn successful_self_read_cursor_uses_only_original_range_inputs() {
    use crate::test_support::MockApiClient;
    for (options, consumes) in [
        (json!({}), true),
        (json!({"test_truncated":true}), true),
        (json!({"offset":1}), false),
        (json!({"limit":1}), false),
        (json!({"test_error":true}), false),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let agents = root.join("repo/pkg/AGENTS.md");
        write(&agents, "SELF_READ_INSTRUCTION");
        let mut input = options.clone();
        input["file_path"] = json!("pkg/AGENTS.md");
        let api = Arc::new(MockApiClient::new(vec![
            read_response("read-self", input),
            read_response("read-other", json!({"file_path":"pkg/main.rs"})),
            finished_response(),
        ]));
        let orch = read_fixture_orchestrator(api.clone(), &root);
        orch.run_turn("read").await.unwrap();
        let calls = api.captured_msgs().await;
        let count = |messages: &[lingxi_core::types::ConversationMessage]| {
            messages
                .iter()
                .filter(|message| {
                    message
                        .text_content()
                        .contains("tool.call hook additional context:")
                })
                .count()
        };
        assert_eq!(count(&calls[1]), 0, "{options}");
        assert_eq!(count(&calls[2]), usize::from(!consumes), "{options}");
        assert!(
            orch.instruction_context_snapshot()
                .await
                .sent_paths
                .contains(&agents),
            "{options}: the later ordinary Read consumes the remaining candidate"
        );
    }
}
