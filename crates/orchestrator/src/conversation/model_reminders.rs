//! Durable model reminders retain their original attachment payloads so replay,
//! compaction and forks can use message identity instead of parsing prose.

use super::*;
use lingxi_core::types::ContentBlock;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub(crate) struct NestedMemoryHistory {
    pub(crate) announced: HashMap<PathBuf, String>,
    pub(crate) nested: HashMap<PathBuf, String>,
}

impl ConversationOrchestrator {
    /// A current text-file attachment owns one durable UUID. Its two native
    /// user rows share a turn, so the first text block carries the API seam.
    pub(crate) fn file_attachment_projection(
        id: MessageId,
        attachment: &serde_json::Value,
    ) -> Option<ConversationMessage> {
        if attachment.get("type")?.as_str()? == "compact_file_reference" {
            let filename = attachment.get("filename")?.as_str()?;
            return Some(ConversationMessage::user_meta(
                id,
                format!(
                    "<system-reminder>\n{}\n</system-reminder>",
                    compact_file_reference_body(Path::new(filename), "Read")
                ),
            ));
        }
        if attachment.get("type")?.as_str()? != "file" {
            return None;
        }
        let filename = attachment.get("filename")?.as_str()?;
        let content = attachment.get("content")?;
        if content.get("type")?.as_str()? != "text" {
            return None;
        }
        let file = content.get("file")?;
        file.get("filePath")?.as_str()?;
        let body = file.get("content")?.as_str()?;
        let start = file.get("startLine")?.as_u64()?;
        let lines = file.get("numLines")?.as_u64()?;
        let total = file.get("totalLines")?.as_u64()?;
        let result = if !body.is_empty() {
            tool_file::read::add_line_numbers(body, start)
        } else if lines >= 1 && total > 1 {
            format!("{start}\t")
        } else if lines >= 1 || total == 0 {
            tool_file::read::EMPTY_FILE_WARNING.into()
        } else {
            format!("<system-reminder>Warning: the file exists but is shorter than the provided offset ({start}). The file has {total} lines.</system-reminder>")
        };
        let input = serde_json::json!({"file_path": filename});
        Some(ConversationMessage::User {
            id,
            content: vec![
                ContentBlock::Text {
                    text: format!("<system-reminder>\nCalled the Read tool with the following input: {input}\n</system-reminder>\n"), citations: None,
                },
                ContentBlock::Text {
                    text: format!("<system-reminder>\nResult of calling the Read tool:\n{result}\n</system-reminder>"), citations: None,
                },
            ],
            is_meta: true,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        })
    }

    /// 2.1.287 nested_memory uses the full, untrimmed acquisition body.
    pub(crate) fn nested_memory_attachment_projection(
        id: MessageId,
        attachment: &serde_json::Value,
    ) -> Option<ConversationMessage> {
        if attachment.get("type")?.as_str()? != "nested_memory" {
            return None;
        }
        attachment.get("path")?.as_str()?;
        let file = attachment.get("content")?;
        let path = file.get("path")?.as_str()?;
        let body = file.get("content")?.as_str()?;
        Some(ConversationMessage::user_meta(
            id,
            format!("<system-reminder>\nContents of {path}:\n\n{body}\n</system-reminder>"),
        ))
    }

    /// O7t/l1e rebuild the baseline from the current model history, rather than
    /// a session-lifetime path cursor. Compacted-away rows cannot suppress a
    /// future attachment; retained rows still suppress an unchanged body.
    pub(crate) async fn nested_memory_history(&self) -> NestedMemoryHistory {
        use lingxi_core::host::instruction_announcements::js_trim;
        use lingxi_core::host::instructions::InstructionFile;
        let raw = {
            let session = self.session.lock().await;
            let history = session.model_context_history();
            let start = history
                .iter()
                .rposition(compaction::is_compact_boundary)
                .unwrap_or(0);
            let attachments = self
                .transcript
                .model_reminder_attachments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            history[start..]
                .iter()
                .filter_map(|message| attachments.get(&message.id()).cloned())
                .collect::<Vec<_>>()
        };
        let mut prior = NestedMemoryHistory::default();
        for attachment in &raw {
            if attachment["type"] != "nested_memory" {
                continue;
            }
            if let (Some(path), Some(body)) = (
                attachment.get("path").and_then(serde_json::Value::as_str),
                attachment
                    .get("content")
                    .and_then(|file| file.get("content"))
                    .and_then(serde_json::Value::as_str),
            ) {
                prior.nested.insert(path.into(), js_trim(body).into());
            }
        }
        let files = |attachment: &serde_json::Value| {
            (attachment["type"] == "instructions"
                && lingxi_core::host::instruction_announcements::is_valid_context_attachment(
                    attachment,
                ))
            .then(|| {
                serde_json::from_value::<Vec<InstructionFile>>(attachment.get("files")?.clone())
                    .ok()
            })
            .flatten()
        };
        if let Some(start) = raw.iter().rposition(|attachment| {
            attachment
                .get("changed")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
                && files(attachment).is_some()
        }) {
            for attachment in &raw[start..] {
                if let Some(files) = files(attachment) {
                    for file in files {
                        prior.announced.insert(file.path.into(), file.content);
                    }
                    if let Some(removed) = attachment.get("removed") {
                        if let Ok(paths) = serde_json::from_value::<Vec<String>>(removed.clone()) {
                            for path in paths {
                                prior.announced.remove(Path::new(&path));
                            }
                        }
                    }
                }
            }
        }
        if !prior.announced.is_empty() {
            let (_, load) = self.main_instruction_load().await;
            match load.get().await {
                Ok(context) => {
                    let current = context
                        .eager_instructions
                        .iter()
                        .flatten()
                        .map(|file| PathBuf::from(&file.path))
                        .collect::<HashSet<_>>();
                    prior.announced.retain(|path, _| current.contains(path));
                }
                Err(error) => {
                    // Native O7t catches failed current instruction acquisition
                    // and returns no baseline, including its nested projection.
                    tracing::debug!(%error, "nested memory history acquisition failed");
                    return NestedMemoryHistory::default();
                }
            }
        }
        prior
    }

    pub(crate) async fn persist_nested_memory_files(
        &self,
        files: Vec<crate::prompt::MemoryFile>,
        trigger: &Path,
        cwd: &Path,
        prior: &NestedMemoryHistory,
    ) -> Vec<ConversationMessage> {
        use lingxi_core::host::instruction_announcements::js_trim;
        let mut messages = Vec::new();
        for file in files {
            let key = memory::lingxi_md::agents::identity(&file.path);
            if self
                .prompt_runtime
                .read_state_map
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&key)
            {
                continue;
            }
            if !self.seed_nested_memory_read_state(&file).await {
                continue;
            }
            let body = js_trim(&file.body);
            if prior.nested.get(&file.path).is_some_and(|old| old == body)
                || prior
                    .announced
                    .get(&file.path)
                    .is_some_and(|old| old == body)
            {
                continue;
            }
            let display_path = crate::prompt::nested_memory::relative_display_path(&file.path, cwd);
            let mut content = serde_json::json!({
                "path": file.path,
                "type": file.tier,
                "content": file.body,
                "contentDiffersFromDisk": file.content_differs_from_disk,
            });
            if let Some(globs) = &file.globs {
                content["globs"] = serde_json::json!(globs);
            }
            if let Some(parent) = &file.parent {
                content["parent"] = serde_json::json!(parent);
            }
            if file.content_differs_from_disk {
                content["rawContent"] = serde_json::json!(file.raw_content);
            }
            let attachment = serde_json::json!({
                "type": "nested_memory",
                "path": file.path,
                "content": content,
                "displayPath": display_path,
            });
            let message = Self::nested_memory_attachment_projection(MessageId::new(), &attachment)
                .expect("new native nested-memory payload");
            messages.push(self.persist_model_reminder(message, attachment).await);
            self.output
                .emit_attachment(lingxi_core::host::AttachmentKind::NestedMemory { display_path })
                .await;
            if self
                .hooks
                .has_hooks_for(&hooks::events::HookEventType::InstructionsLoaded)
                .await
            {
                let hooks = self.hooks.clone();
                let ctx = self.lifecycle_hook_ctx(false).await;
                let event = HookEvent::InstructionsLoaded {
                    file_path: file.path,
                    memory_type: file.tier,
                    load_reason: if file.globs.is_some() {
                        hooks::events::InstructionsLoadReason::PathGlobMatch
                    } else if file.parent.is_some() {
                        hooks::events::InstructionsLoadReason::Include
                    } else {
                        hooks::events::InstructionsLoadReason::NestedTraversal
                    },
                    globs: file.globs,
                    trigger_file_path: Some(trigger.into()),
                    parent_file_path: file.parent,
                };
                // Native m7t invokes async Wan without awaiting the hook Promise.
                tokio::spawn(async move {
                    let _ = hooks.execute(event, ctx).await;
                });
            }
        }
        messages
    }

    pub(crate) async fn persist_model_reminder(
        &self,
        message: ConversationMessage,
        attachment: serde_json::Value,
    ) -> ConversationMessage {
        {
            let mut session = self.session.lock().await;
            self.transcript
                .model_reminder_attachments
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(message.id(), attachment);
            session.history.push(message.clone());
        }
        self.persist_message_to_jsonl(&message).await;
        message
    }

    pub(crate) async fn mcp_instructions_reminder_message(&self) -> Option<ConversationMessage> {
        let registry = self.mcp_registry.as_ref()?;
        let (names, current) = registry.server_instruction_snapshot().await;
        let prior =
            {
                let session = self.session.lock().await;
                let attachments = self
                    .transcript
                    .model_reminder_attachments
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                session.model_context_history().iter()
                .filter_map(|message| attachments.get(&message.id()))
                .map(|attachment| serde_json::json!({"type":"attachment", "attachment":attachment}))
                .collect::<Vec<_>>()
            };
        let attachment = crate::prompt::mcp_instructions::attachment(
            &current,
            &names,
            &crate::prompt::mcp_instructions::BUILTIN_CLIENT_BLOCKS,
            &prior,
            mcp::discovery_cache::feature_enabled(),
        )?;
        let content = crate::prompt::mcp_instructions::render(&attachment)?;
        Some(
            self.persist_model_reminder(
                ConversationMessage::user_meta(MessageId::new(), content),
                attachment,
            )
            .await,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream,
        NoOpPermissionGate, StaticMemoryProvider,
    };
    use platform_posix::{fs::PosixFileSystem, mcp::PosixMcpTransport};
    use serde_json::json;
    use tool_api::registry::ToolRegistry;

    #[test]
    fn file_attachment_projection_matches_current_native_renderer_and_api_merge() {
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/file_attachment_2_1_287.json"
        ))
        .unwrap();
        assert_eq!(oracle["cases"].as_array().unwrap().len(), 6);
        for case in oracle["cases"].as_array().unwrap() {
            let id = MessageId::new();
            let projected =
                ConversationOrchestrator::file_attachment_projection(id, &case["input"]).unwrap();
            let native_rows = case["expected"]["rendered"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
                .map(|(index, text)| {
                    ConversationMessage::user_meta(
                        if index == 0 { id } else { MessageId::new() },
                        text.as_str().unwrap().into(),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(
                llm_runtime::convert::normalize_messages_for_api(vec![projected]),
                llm_runtime::convert::normalize_messages_for_api(native_rows),
                "{}",
                case["name"]
            );
        }
    }

    #[test]
    fn nested_memory_rendering_matches_actual_287_helper_fixtures() {
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/nested_memory_2_1_287.json"
        ))
        .unwrap();
        assert_eq!(oracle["cases"].as_array().unwrap().len(), 9);
        for case in oracle["cases"].as_array().unwrap() {
            let rendered = case["expected"]["attachments"]
                .as_array()
                .unwrap()
                .iter()
                .map(|attachment| {
                    ConversationOrchestrator::nested_memory_attachment_projection(
                        MessageId::new(),
                        attachment,
                    )
                    .unwrap()
                    .text_content()
                })
                .collect::<Vec<_>>();
            assert_eq!(serde_json::json!(rendered), case["expected"]["rendered"]);
        }
    }

    fn orchestrator(api: Arc<MockApiClient>, dir: &std::path::Path) -> ConversationOrchestrator {
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            api,
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.to_owned(),
        )
    }

    #[tokio::test]
    async fn nested_history_cutoff_requires_a_typed_compact_boundary() {
        for typed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let root = orchestrator(Arc::new(MockApiClient::new(vec![])), dir.path());
            let path = dir.path().join("pkg").join(branding::MEMORY_FILE);
            let attachment = json!({
                "type": "nested_memory",
                "path": path,
                "content": {
                    "path": path,
                    "type": "Project",
                    "content": "Folder guide\n",
                    "contentDiffersFromDisk": false,
                },
                "displayPath": format!("pkg/{}", branding::MEMORY_FILE),
            });
            let nested = ConversationOrchestrator::nested_memory_attachment_projection(
                MessageId::new(),
                &attachment,
            )
            .unwrap();
            let system = ConversationMessage::System {
                id: MessageId::new(),
                content: if typed {
                    "arbitrary typed boundary body".into()
                } else {
                    compaction::BOUNDARY_CONTENT.into()
                },
                subtype: typed.then(|| "compact_boundary".into()),
                compact_metadata: None,
                model_fallback: None,
                refusal_fallback: None,
            };
            let history = vec![
                nested.clone(),
                system,
                ConversationMessage::user(MessageId::new(), "continue".into()),
            ];
            root.session.lock().await.history = history.clone();
            root.transcript
                .model_reminder_attachments
                .lock()
                .unwrap()
                .insert(nested.id(), attachment);
            let prior = root.nested_memory_history().await;
            assert_eq!(
                prior.nested.get(&path).map(String::as_str),
                (!typed).then_some("Folder guide"),
                "the body does not identify a boundary: typed={typed}"
            );
            assert_eq!(root.session.lock().await.model_context_history(), history);
        }
    }

    #[tokio::test]
    async fn connected_computer_use_without_server_instructions_announces_builtin_block_once() {
        let dir = tempfile::tempdir().unwrap();
        let transport = Arc::new(PosixMcpTransport::new());
        let registry = Arc::new(mcp::McpRegistry::with_raw_conn(
            transport.clone(),
            transport,
        ));
        let script = r#"import sys,json
for line in sys.stdin:
 r=json.loads(line)
 if 'id' not in r: continue
 if r.get('method')=='server/discover':
  v={'supportedVersions':['2026-07-28'],'capabilities':{}}
 elif r.get('method')=='initialize':
  v={'protocolVersion':'2025-11-25','capabilities':{},'serverInfo':{'name':'computer-use','version':'286'}}
 else: v={}
 print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':v}),flush=True)
"#;
        let config = mcp::build_server_from_json_entry(
            "computer-use",
            &json!({"command":"python3", "args":["-u","-c",script]}),
            mcp::ConfigScope::Dynamic,
        )
        .unwrap();
        registry.connect(config).await.unwrap();
        let (eligible, server_instructions) = registry.server_instruction_snapshot().await;
        assert_eq!(eligible, vec!["computer-use"]);
        assert!(server_instructions.is_empty());
        let response = mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "ok".into(),
                cache_control: None, citations: None,
            }],
            Some("end_turn"),
        );
        let api = Arc::new(MockApiClient::new(vec![response.clone(), response]));
        let orch = orchestrator(api.clone(), dir.path()).with_mcp_registry(registry);
        orch.run_turn("first").await.unwrap();
        orch.run_turn("second").await.unwrap();
        let calls = api.captured_msgs().await;
        let expected = crate::prompt::mcp_instructions::render(&json!({
            "type":"mcp_instructions_delta",
            "addedNames":["computer-use"],
            "addedBlocks":[format!(
                "## computer-use\n{}",
                crate::prompt::mcp_instructions::BUILTIN_CLIENT_BLOCKS[0].block
            )],
            "removedNames":[],
        }))
        .unwrap();
        let instructions = |messages: &[ConversationMessage]| {
            messages
                .iter()
                .filter(|message| message.text_content() == expected)
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(instructions(&calls[0]).len(), 1);
        assert_eq!(instructions(&calls[0]), instructions(&calls[1]));
        let attachments = orch
            .transcript
            .model_reminder_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mcp_attachments: Vec<_> = attachments
            .values()
            .filter(|attachment| attachment["type"] == "mcp_instructions_delta")
            .collect();
        assert_eq!(mcp_attachments.len(), 1);
        let payload = mcp_attachments[0];
        assert_eq!(payload["addedNames"], json!(["computer-use"]));
        if mcp::discovery_cache::feature_enabled() {
            assert_eq!(payload["addedServerInstructions"], json!([""]));
        }
    }

    #[tokio::test]
    async fn live_mcp_instructions_survive_requests_resume_and_disconnect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let transport = Arc::new(PosixMcpTransport::new());
        let registry = Arc::new(mcp::McpRegistry::with_raw_conn(
            transport.clone(),
            transport,
        ));
        let script = r#"import sys,json
for line in sys.stdin:
 r=json.loads(line)
 if 'id' not in r: continue
 method=r.get('method')
 if method=='server/discover':
  v={'supportedVersions':['2026-07-28'],'capabilities':{},'instructions':'Read the project index first.'}
 elif method=='initialize':
  v={'protocolVersion':'2025-11-25','capabilities':{},'serverInfo':{'name':'records','version':'286'},'instructions':'Read the project index first.'}
 else: v={}
 print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':v}),flush=True)
"#;
        let config = mcp::build_server_from_json_entry(
            "records",
            &json!({
                "command":"python3", "args":["-u","-c",script]
            }),
            mcp::ConfigScope::Dynamic,
        )
        .unwrap();
        registry.connect(config).await.unwrap();
        let client = registry.get_client("records").await.unwrap();
        assert_eq!(
            client.server_instructions().await.as_deref(),
            Some("Read the project index first.")
        );
        assert!(client.server_capabilities().await.is_some());
        let response = mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "ok".into(),
                cache_control: None, citations: None,
            }],
            Some("end_turn"),
        );
        let api = Arc::new(MockApiClient::new(vec![
            response.clone(),
            response.clone(),
            response,
        ]));
        let fs: Arc<dyn lingxi_core::host::FileSystem> =
            Arc::new(PosixFileSystem::new(dir.path().to_owned()));
        let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(path.clone(), fs));
        let orch = orchestrator(api.clone(), dir.path())
            .with_mcp_registry(registry.clone())
            .with_jsonl_writer(writer);
        orch.run_turn("first").await.unwrap();
        orch.run_turn("second").await.unwrap();
        let calls = api.captured_msgs().await;
        let instructions = |messages: &[ConversationMessage]| {
            messages
                .iter()
                .filter(|message| message.text_content().contains("# MCP Server Instructions"))
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            instructions(&calls[0]),
            instructions(&calls[1]),
            "same durable message, no reannouncement"
        );
        assert_eq!(instructions(&calls[0]).len(), 1);
        let raw = std::fs::read_to_string(&path).unwrap();
        let rows: Vec<session::JsonlMessage> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let attachment = rows
            .iter()
            .find(|row| {
                row.extra
                    .get("attachment")
                    .and_then(|a| a.get("type"))
                    .and_then(serde_json::Value::as_str)
                    == Some("mcp_instructions_delta")
            })
            .unwrap();
        assert_eq!(attachment.message_type, "attachment");
        assert!(attachment.message.is_null());
        assert!(!attachment.extra.contains_key("permissionMode"));
        assert!(!attachment.extra.contains_key("isMeta"));
        assert!(attachment.prompt_id.is_none());
        assert_eq!(
            attachment.uuid,
            instructions(&calls[0])[0].id().as_uuid().to_string()
        );
        let resumed = orchestrator(Arc::new(MockApiClient::new(vec![])), dir.path())
            .with_mcp_registry(registry.clone());
        *resumed.session.lock().await = crate::resume::state_from_messages(
            orch.session.lock().await.session_id.as_uuid(),
            &rows,
        );
        resumed.restore_resume_runtime_metadata(&rows).await;
        assert!(resumed.mcp_instructions_reminder_message().await.is_none());
        registry.disconnect("records").await.unwrap();
        orch.run_turn("third").await.unwrap();
        let calls = api.captured_msgs().await;
        assert_eq!(
            instructions(&calls[2]).len(),
            1,
            "prior instructions remain with a correction"
        );
        assert!(calls[2]
            .iter()
            .any(|message| message.text_content().contains(
                "servers have disconnected. Their instructions above no longer apply:\nrecords"
            )));
        // Removing the retained attachment (as compaction does) also removes
        // its delta baseline; replay never relies on a process-global latch.
        resumed.session.lock().await.history.clear();
        assert!(resumed.mcp_instructions_reminder_message().await.is_none());
    }

    #[tokio::test]
    async fn total_token_attachment_round_trips_without_rewriting_old_requests() {
        let dir = tempfile::tempdir().unwrap();
        let orch = orchestrator(Arc::new(MockApiClient::new(vec![])), dir.path());
        let body = "<total_tokens>42</total_tokens>";
        let message = orch
            .persist_model_reminder(
                ConversationMessage::user_meta(
                    MessageId::new(),
                    format!("<system-reminder>\n{body}\n</system-reminder>"),
                ),
                json!({"type":"total_tokens_reminder", "text":body}),
            )
            .await;
        let row = orch.to_jsonl_message(
            &message,
            &uuid::Uuid::nil().to_string(),
            None,
            None,
            None,
            None,
        );
        assert_eq!(row.message_type, "attachment");
        assert!(row.message.is_null());
        let restored = crate::resume::state_from_messages(uuid::Uuid::nil(), &[row]);
        assert_eq!(restored.history, vec![message]);
    }
}
