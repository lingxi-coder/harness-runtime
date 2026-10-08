use super::*;
use crate::OrchestratorConfig;
use crate::test_support::{
    MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider, noop_hook_executor,
};
use platform_posix::fs::PosixFileSystem;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;
/// Frame buffering holds `tool_result` frames until the collection point
/// releases them, so the SDK sees RECEIVED order rather than completion
/// order — and so a cancelled tool reports its synthetic instead of the
/// real outcome the executor discarded.
///
/// These pin the mechanism. They do NOT prove the streaming driver's
/// ordering end-to-end; that would need two tools whose completion order
/// differs from their received order.
mod tool_frame_ordering_tests {
    use super::*;

    fn orch_for_frames(output: Arc<MockOutputStream>) -> ConversationOrchestrator {
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output,
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
    }

    fn result_ids(events: &[lingxi_core::host::orchestrator::OutputEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                lingxi_core::host::orchestrator::OutputEvent::ToolResult { id, .. } => {
                    Some(id.to_string())
                }
                _ => None,
            })
            .collect()
    }

    /// With buffering OFF the frame goes straight out — the batched driver,
    /// which dispatches in received order anyway, is unchanged.
    #[tokio::test]
    async fn buffering_off_emits_immediately() {
        let output = Arc::new(MockOutputStream::new());
        let orch = orch_for_frames(output.clone());
        let id = lingxi_core::types::ToolUseId::new();
        orch.emit_tool_result_frame(&id, "Bash", "out", &serde_json::json!({}), None, None)
            .await;
        assert_eq!(result_ids(&output.snapshot().await), vec![id.to_string()]);
    }

    #[tokio::test]
    async fn mod_result_stage_captures_only_its_tool_id() {
        let output = Arc::new(MockOutputStream::new());
        let orch = orch_for_frames(output.clone());
        let staged = lingxi_core::types::ToolUseId::new();
        let nested = lingxi_core::types::ToolUseId::new();
        let (_, stage) = crate::conversation::with_mod_result_stage(&staged, async {
            orch.record_tool_use_result(&staged, serde_json::json!("staged"))
                .await;
            orch.emit_tool_result_frame(
                &staged,
                "Bash",
                "staged",
                &serde_json::json!("staged"),
                None,
             None)
            .await;
            orch.record_tool_use_result(&nested, serde_json::json!("nested"))
                .await;
            orch.emit_tool_result_frame(
                &nested,
                "Read",
                "nested",
                &serde_json::json!("nested"),
                None,
             None)
            .await;
        })
        .await;
        assert_eq!(stage.tool_use_result, Some(serde_json::json!("staged")));
        assert_eq!(
            result_ids(&output.snapshot().await),
            vec![nested.to_string()]
        );
        assert_eq!(
            orch.transcript
                .tool_use_results
                .lock()
                .await
                .get(nested.as_str()).map(|projection| &projection.value),
            Some(&serde_json::json!("nested"))
        );
        orch.commit_mod_result_stage(&staged, "Bash", stage, None)
            .await;
        assert_eq!(
            result_ids(&output.snapshot().await),
            vec![nested.to_string(), staged.to_string()]
        );
    }

    #[tokio::test]
    async fn mod_replacement_keeps_structured_error_result_in_sdk_frame() {
        let output = Arc::new(MockOutputStream::new());
        let orch = orch_for_frames(output.clone());
        let id = lingxi_core::types::ToolUseId::new();
        let (_, stage) = crate::conversation::with_mod_result_stage(&id, async {
            orch.record_tool_use_result(&id, serde_json::json!({"interrupted":false}))
                .await;
            orch.emit_tool_result_frame(
                &id,
                "Bash",
                "core",
                &serde_json::json!({"interrupted":false}),
                None,
             None)
            .await;
        })
        .await;
        let replacement = serde_json::json!({"interrupted":true,"stdout":"partial"});
        orch.commit_mod_result_stage(
            &id,
            "Bash",
            stage,
            Some((lingxi_core::host::ToolResultProjection {
                data: replacement.clone().into(),
                content: serde_json::json!("partial").into(),
                model_text: Some(serde_json::json!("partial").into()),
                mcp_meta: None,
            }, "partial".into())),
        )
        .await;
        assert_eq!(
            orch.transcript
                .tool_use_results
                .lock()
                .await
                .get(id.as_str()).map(|projection| &projection.value),
            Some(&replacement)
        );
        assert!(output.snapshot().await.iter().any(|event| matches!(
            event,
            lingxi_core::host::orchestrator::OutputEvent::ToolResult { id: got_id, result, .. }
                if got_id == &id && result == &replacement
        )));
    }

    /// With buffering ON nothing reaches the stream until release.
    #[tokio::test]
    async fn buffered_frames_are_withheld_then_released_in_caller_order() {
        let output = Arc::new(MockOutputStream::new());
        let orch = orch_for_frames(output.clone());
        orch.set_tool_frame_buffering(true).await;

        let first = lingxi_core::types::ToolUseId::new();
        let second = lingxi_core::types::ToolUseId::new();
        // Buffered in COMPLETION order: `second` finished first.
        orch.emit_tool_result_frame(&second, "Bash", "b", &serde_json::json!({}), None, None)
            .await;
        orch.emit_tool_result_frame(&first, "Read", "a", &serde_json::json!({}), None, None)
            .await;
        assert!(
            result_ids(&output.snapshot().await).is_empty(),
            "nothing may reach the stream while buffering is on"
        );

        // Released in RECEIVED order by the collection point.
        orch.release_tool_frame(&first, "Read", "a", false).await;
        orch.release_tool_frame(&second, "Bash", "b", false).await;
        assert_eq!(
            result_ids(&output.snapshot().await),
            vec![first.to_string(), second.to_string()],
            "release order wins over completion order"
        );
    }

    /// A tool that never dispatched (queued, then cancelled) has no buffered
    /// frame and still gets one. Before the collection point released
    /// frames, this case emitted nothing at all.
    #[tokio::test]
    async fn releasing_an_undispatched_tool_still_emits() {
        let output = Arc::new(MockOutputStream::new());
        let orch = orch_for_frames(output.clone());
        orch.set_tool_frame_buffering(true).await;

        let id = lingxi_core::types::ToolUseId::new();
        orch.release_tool_frame(&id, "Read", "The user doesn't want to proceed", true)
            .await;
        assert_eq!(
            result_ids(&output.snapshot().await),
            vec![id.to_string()],
            "a never-dispatched tool must still report a frame"
        );
    }

    /// The released content is the block's FINAL text, so a synthetic that
    /// replaced a cancelled tool's real outcome wins over what dispatch
    /// buffered.
    #[tokio::test]
    async fn substituted_content_wins_over_the_buffered_result() {
        let output = Arc::new(MockOutputStream::new());
        let orch = orch_for_frames(output.clone());
        orch.set_tool_frame_buffering(true).await;

        let id = lingxi_core::types::ToolUseId::new();
        orch.emit_tool_result_frame(
            &id,
            "Bash",
            "REAL OUTPUT",
            &serde_json::json!({ "stdout": "REAL OUTPUT" }),
            None,
         None)
        .await;
        orch.release_tool_frame(&id, "Bash", "SYNTHETIC", true)
            .await;

        let events = output.snapshot().await;
        let found = events.iter().any(|e| matches!(
            e,
            lingxi_core::host::orchestrator::OutputEvent::ToolResult { id: gid, .. } if gid.to_string() == id.to_string()
        ));
        assert!(found, "the released frame must be emitted");
        assert!(
            !format!("{events:?}").contains("REAL OUTPUT"),
            "the discarded real outcome must not reach the SDK: {events:?}"
        );
    }

    /// A substituted non-MCP synthetic must override the dispatch-side
    /// `interrupted` denial kind with the final `user-rejected` provenance.
    #[tokio::test]
    async fn substituted_non_mcp_frame_uses_rewritten_denial_kind() {
        let output = Arc::new(MockOutputStream::new());
        let orch = orch_for_frames(output.clone());
        orch.set_tool_frame_buffering(true).await;

        let id = lingxi_core::types::ToolUseId::new();
        orch.emit_tool_result_frame(
            &id,
            "Bash",
            "REAL OUTPUT",
            &serde_json::json!({ "error": "aborted" }),
            Some("interrupted"),
         None)
        .await;
        orch.record_tool_denial_kind(&id, "user-rejected").await;
        orch.record_tool_use_result(
            &id,
            serde_json::Value::String("User rejected tool use".into()),
        )
        .await;

        orch.release_tool_frame(&id, "Bash", "SYNTHETIC", true)
            .await;

        assert_eq!(
            output.denial_snapshot().await,
            vec![(id.clone(), "user-rejected".to_string())]
        );
        assert!(
            !format!("{:?}", output.snapshot().await).contains("REAL OUTPUT"),
            "the discarded real outcome must not leak into the SDK frame"
        );
    }

    /// A queued MCP cancellation never buffered a dispatch frame, but still
    /// must emit the final `interrupted` provenance and keep the tool name.
    #[tokio::test]
    async fn undispatched_mcp_cancelled_tool_uses_recorded_metadata() {
        let output = Arc::new(MockOutputStream::new());
        let orch = orch_for_frames(output.clone());
        orch.set_tool_frame_buffering(true).await;

        let id = lingxi_core::types::ToolUseId::new();
        orch.record_tool_denial_kind(&id, "interrupted").await;
        orch.record_tool_use_result(&id, serde_json::Value::String("Error: interrupted".into()))
            .await;

        orch.release_tool_frame(&id, "McpCancelTool", "Error: interrupted", true)
            .await;

        assert_eq!(
            output.denial_snapshot().await,
            vec![(id.clone(), "interrupted".to_string())]
        );
        let events = output.snapshot().await;
        assert!(matches!(
            events.as_slice(),
            [lingxi_core::host::orchestrator::OutputEvent::ToolResult { id: got_id, tool, .. }]
                if got_id == &id && tool == "McpCancelTool"
        ));
    }

    #[tokio::test]
    async fn no_writer_persist_consumes_frame_side_tables() {
        let output = Arc::new(MockOutputStream::new());
        let orch = orch_for_frames(output);
        orch.set_tool_frame_buffering(true).await;
        let id = lingxi_core::types::ToolUseId::new();
        orch.record_tool_use_result(&id, serde_json::json!({"ok": true}))
            .await;
        orch.record_tool_denial_kind(&id, "user-rejected").await;
        orch.record_tool_use_mcp_meta(&id, serde_json::json!({"source": "test"}))
            .await;
        orch.record_source_tool_assistant_uuid(&id, "assistant-line".into())
            .await;
        orch.release_tool_frame(&id, "McpTool", "cancelled", true)
            .await;

        let message = lingxi_core::types::ConversationMessage::User { api_message_override: None,
            id: lingxi_core::types::MessageId::new(),
            content: vec![lingxi_core::types::ContentBlock::ToolResult { content_projection: None,
                tool_use_id: id.clone(),
                content: "cancelled".into(),
                is_error: Some(true),
                provider_tool_use_id: None,
                content_blocks: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        orch.persist_message_to_jsonl(&message).await;

        let key = id.to_string();
        assert!(
            !orch
                .transcript
                .tool_use_results
                .lock()
                .await
                .contains_key(&key)
        );
        assert!(
            !orch
                .transcript
                .tool_denial_kinds
                .lock()
                .await
                .contains_key(&key)
        );
        assert!(
            !orch
                .transcript
                .tool_use_mcp_meta
                .lock()
                .await
                .contains_key(&key)
        );
        assert!(
            !orch
                .transcript
                .tool_source_assistant_uuids
                .lock()
                .await
                .contains_key(&key)
        );
    }

    #[tokio::test]
    async fn abandoning_a_buffered_frame_consumes_its_side_tables() {
        let orch = orch_for_frames(Arc::new(MockOutputStream::new()));
        orch.set_tool_frame_buffering(true).await;
        let id = lingxi_core::types::ToolUseId::new();
        orch.emit_tool_result_frame(
            &id,
            "Bash",
            "out",
            &serde_json::json!({"stdout": "out"}),
            Some("interrupted"),
         None)
        .await;
        orch.record_tool_use_result(&id, serde_json::json!({"stdout": "out"}))
            .await;
        orch.record_tool_denial_kind(&id, "interrupted").await;

        orch.set_tool_frame_buffering(false).await;

        let key = id.to_string();
        assert!(
            !orch
                .transcript
                .tool_use_results
                .lock()
                .await
                .contains_key(&key)
        );
        assert!(
            !orch
                .transcript
                .tool_denial_kinds
                .lock()
                .await
                .contains_key(&key)
        );
    }
}

fn orch_with_writer(dir: &std::path::Path, path: std::path::PathBuf) -> ConversationOrchestrator {
    let fs: Arc<dyn lingxi_core::host::FileSystem> =
        Arc::new(PosixFileSystem::new(dir.to_path_buf()));
    let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(path, fs));
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.to_path_buf(),
    )
    .with_jsonl_writer(writer)
}

/// A hook-run attachment lands as its own `type:"attachment"` transcript
/// line whose payload rides in the `attachment` key BEFORE `type`, and it
/// advances the chain so the next line parents to it.
#[tokio::test]
async fn hook_attachment_is_persisted_as_an_attachment_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), path.clone());

    let payload = hooks::success_attachment(
        &hooks::HookAttachmentIdentity {
            hook_name: "PostToolUse:Bash".into(),
            hook_event: "PostToolUse".into(),
            tool_use_id: "toolu_01ApkBwAZMCAza47B5nAWiGS".into(),
        },
        &hooks::ExactHookText::from_text("formatted"),
        "formatted\n",
        "",
        0,
        "./hooks/fmt.sh",
        37,
    );
    let expected_payload = payload.value.clone();
    let overrides = payload
        .strings
        .iter()
        .map(|sidecar| {
            (
                format!("/attachment{}", sidecar.pointer),
                sidecar.code_units.clone(),
            )
        })
        .collect();
    orch.persist_hook_attachment_to_jsonl(payload.value, overrides).await;

    let raw = std::fs::read_to_string(&path).expect("read jsonl");
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "one line: {raw}");
    let v: serde_json::Value = serde_json::from_str(lines[0]).expect("json");
    assert_eq!(v["type"], "attachment");
    assert_eq!(v["attachment"], expected_payload);
    assert!(v.get("message").is_none(), "no inner message: {}", lines[0]);
    // Payload precedes the discriminator on real 2.1.220 attachment lines.
    let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        &keys[..4],
        ["parentUuid", "isSidechain", "attachment", "type"]
    );
    // Chain advanced.
    assert_eq!(
        orch.transcript.last_jsonl_uuid.lock().await.as_deref(),
        v["uuid"].as_str()
    );
}

/// O3: attachments queued during a tool dispatch are flushed — in
/// production order, as `attachment` lines — AFTER the `tool_result` they
/// follow, matching claude's stream order (`insertMessageChain` writes the
/// yielded attachment message right after the yielded tool_result), and the
/// queue is drained so a second flush is a no-op.
#[tokio::test]
async fn queued_hook_attachments_flush_after_the_tool_result_in_order() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), path.clone());

    let tuid = lingxi_core::types::ToolUseId::new();
    orch.queue_hook_attachment(
        &tuid,
        hooks::additional_context_attachment(
                "PostToolUse:Edit",
                tuid.as_str(),
                "PostToolUse",
                &["FIRST".into()],
            ),
        None,
    )
    .await;
    orch.queue_hook_attachment(
        &tuid,
        lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
            hooks::error_during_execution_attachment(
                "SECOND",
                "PostToolUse:Edit",
                tuid.as_str(),
                "PostToolUse",
            ),
        ),
        None,
    )
    .await;

    let msg = ConversationMessage::User { api_message_override: None,
        id: lingxi_core::types::MessageId::new(),
        content: vec![lingxi_core::types::ContentBlock::ToolResult { content_projection: None,
            tool_use_id: tuid.clone(),
            content: "ok".into(),
            is_error: Some(false),
            provider_tool_use_id: None,
            content_blocks: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    orch.persist_message_to_jsonl(&msg).await;
    orch.flush_hook_attachments(&tuid).await;
    // Draining: a second flush writes nothing.
    orch.flush_hook_attachments(&tuid).await;

    let raw = std::fs::read_to_string(&path).expect("read jsonl");
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 3, "tool_result + 2 attachments: {raw}");
    let v0: serde_json::Value = serde_json::from_str(lines[0]).expect("json");
    let v1: serde_json::Value = serde_json::from_str(lines[1]).expect("json");
    let v2: serde_json::Value = serde_json::from_str(lines[2]).expect("json");
    assert_eq!(v0["type"], "user");
    assert_eq!(v1["attachment"]["type"], "hook_additional_context");
    assert_eq!(v1["attachment"]["content"][0], "FIRST");
    assert_eq!(v2["attachment"]["type"], "hook_error_during_execution");
    assert_eq!(v2["attachment"]["content"], "SECOND");
    // Linear chain: result → first attachment → second attachment.
    assert_eq!(v1["parentUuid"], v0["uuid"]);
    assert_eq!(v2["parentUuid"], v1["uuid"]);
}

#[tokio::test]
async fn discarded_result_cleanup_removes_only_its_queued_attachments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), path.clone());
    let accepted = lingxi_core::types::ToolUseId::from("accepted-tool");
    let discarded = lingxi_core::types::ToolUseId::from("discarded-tool");
    let attachment = |id: &lingxi_core::types::ToolUseId, text: &str| {
        hooks::additional_context_attachment(
                "PostToolUse:Edit",
                id.as_str(),
                "PostToolUse",
                &[hooks::ExactHookText::from_text(text)],
            )
    };
    orch.queue_hook_attachment(&accepted, attachment(&accepted, "keep"), None)
        .await;
    orch.queue_hook_attachment(&discarded, attachment(&discarded, "drop"), None)
        .await;

    // Server-fallback tombstones use the same cleanup path as a synthetic
    // replacing a real completion. It must preserve the already accepted id.
    orch.clear_discarded_tool_result_metadata(&discarded).await;
    let queued = orch.transcript.pending_hook_attachments.lock().await;
    assert!(queued.contains_key(accepted.as_str()));
    assert!(!queued.contains_key(discarded.as_str()));
    drop(queued);

    let accepted_result = ConversationMessage::User { api_message_override: None,
        id: lingxi_core::types::MessageId::new(),
        content: vec![lingxi_core::types::ContentBlock::ToolResult { content_projection: None,
            tool_use_id: accepted.clone(),
            content: "accepted".into(),
            is_error: Some(false),
            provider_tool_use_id: None,
            content_blocks: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    orch.persist_message_to_jsonl(&accepted_result).await;
    orch.flush_hook_attachments(&accepted).await;
    orch.flush_hook_attachments(&discarded).await;

    let raw = std::fs::read_to_string(path).expect("read transcript");
    let lines: Vec<&str> = raw.lines().filter(|line| !line.trim().is_empty()).collect();
    assert_eq!(lines.len(), 2, "only accepted result and attachment persist");
    let attachment_line: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(attachment_line["attachment"]["content"][0], "keep");
}

/// END-TO-END: a real `HookExecutorImpl` wired with the real sink and a
/// real orchestrator writes ONE `attachment` transcript line for the hook
/// run — the whole publish chain (executor → sink → JSONL writer), not just
/// each half. Guards against the value being computed but never persisted.
#[tokio::test]
async fn executor_run_reaches_the_transcript_through_the_real_sink() {
    use async_trait::async_trait;
    use hooks::executor::BuiltinHookHandler;
    use hooks::registry::{HookContext, HookRegistry};
    use hooks::{HookOutcome, HookResult};

    struct Ok0;
    #[async_trait]
    impl BuiltinHookHandler for Ok0 {
        async fn handle(&self, _event: &hooks::HookEvent, _ctx: &HookContext) -> HookResult {
            HookResult {
                outcome: HookOutcome::Success,
                stdout: "linted".into(),
                stderr: String::new(),
                exit_code: Some(0),
                response: None,
            }
        }
        fn id(&self) -> &str {
            "lint"
        }
    }

    struct UnusedHttp;
    #[async_trait]
    impl lingxi_core::host::HttpTransport for UnusedHttp {
        async fn request(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            Err(lingxi_core::host::HttpError::InvalidRequest(
                "unused".into(),
            ))
        }
    }
    struct UnusedRuntime;
    #[async_trait]
    impl lingxi_core::host::RuntimeSpawner for UnusedRuntime {
        async fn spawn(
            &self,
            _name: &str,
            _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError>
        {
            Err(lingxi_core::host::RuntimeError::Internal("unused".into()))
        }
        async fn sleep(&self, _d: std::time::Duration) {}
        async fn cancel(
            &self,
            _h: &lingxi_core::host::BackgroundTaskHandle,
        ) -> Result<(), lingxi_core::host::RuntimeError> {
            Ok(())
        }
    }

    let mut registry = HookRegistry::new();
    registry.register(hooks::HookDefinition {
        id: lingxi_core::types::HookId::new(),
        name: "lint".into(),
        events: vec![hooks::HookEventType::PostToolUse],
        if_condition: None,
        executor: hooks::HookExecutor::Builtin {
            handler_id: "lint".into(),
        },
        source: hooks::HookSource::Settings(lingxi_core::types::SettingsScope::User),
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
    });

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("session.jsonl");
    let sink = Arc::new(crate::JsonlHookAttachmentSink::new());

    let mut exec = hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(UnusedHttp),
        Arc::new(UnusedRuntime),
    )
    .with_attachment_sink(sink.clone() as Arc<dyn hooks::HookAttachmentSink>);
    exec.register_builtin(Arc::new(Ok0));

    let orch = Arc::new(orch_with_writer(dir.path(), path.clone()));
    sink.attach(&orch);

    exec.execute(
        hooks::HookEvent::PostToolUse {
            tool_name: "Edit".into(),
            tool_input: serde_json::json!({}),
            tool_output: serde_json::json!({}),
            tool_use_id: lingxi_core::types::ToolUseId::from("toolu_e2e".to_string()),
            duration_ms: None,
        },
        HookContext::default(),
    )
    .await;

    let raw = std::fs::read_to_string(&path).expect("transcript written");
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "exactly one attachment line: {raw}");
    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["type"], "attachment");
    assert_eq!(v["attachment"]["type"], "hook_success");
    assert_eq!(v["attachment"]["hookName"], "PostToolUse:Edit");
    assert_eq!(v["attachment"]["hookEvent"], "PostToolUse");
    assert_eq!(v["attachment"]["toolUseID"], "toolu_e2e");
    assert_eq!(v["attachment"]["content"], "linted");
    assert_eq!(v["attachment"]["exitCode"], 0);
    assert_eq!(v["attachment"]["command"], "lint");
}

/// The sink adapter forwards to the orchestrator once attached, and is an
/// inert no-op before that (composition order: the hook executor is built
/// before the orchestrator exists).
#[tokio::test]
async fn sink_forwards_to_the_attached_orchestrator() {
    use hooks::HookAttachmentSink;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("session.jsonl");
    let sink = Arc::new(crate::JsonlHookAttachmentSink::new());

    // Unattached: must not panic, must not write.
    sink.record(lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
        serde_json::json!({"type": "hook_success"}),
    ))
        .await;
    assert!(!path.exists(), "unattached sink writes nothing");

    let orch = Arc::new(orch_with_writer(dir.path(), path.clone()));
    sink.attach(&orch);
    sink.record(lingxi_core::types::utf16_json::Utf16JsonProjection::plain(
        serde_json::json!({"type": "hook_cancelled"}),
    ))
        .await;

    let raw = std::fs::read_to_string(&path).expect("read jsonl");
    assert!(
        raw.contains(r#""attachment":{"type":"hook_cancelled"}"#),
        "sink persisted the payload: {raw}"
    );
}

#[tokio::test]
async fn sink_atomically_persists_oversized_hook_output_in_session_storage() {
    use hooks::HookAttachmentSink;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("session.jsonl");
    let sink = Arc::new(crate::JsonlHookAttachmentSink::new());
    let orch =
        Arc::new(orch_with_writer(dir.path(), path).with_config_home(dir.path().to_path_buf()));
    let session_uuid = orch.session.lock().await.session_id.as_uuid().to_string();
    sink.attach(&orch);
    let body = hooks::ExactHookText::from_text(
        "x".repeat(hooks::attachment::HOOK_OUTPUT_INLINE_LIMIT + 1),
    );

    let reference = sink
        .persist_large_output(&body)
        .await
        .expect("persisted output");
    let output_dir = session::jsonl::path::tool_results_dir(
        dir.path(),
        &orch.current_cwd().to_string_lossy(),
        &session_uuid,
    );
    let files = std::fs::read_dir(output_dir)
        .expect("tool-results directory")
        .collect::<Result<Vec<_>, _>>()
        .expect("tool-results entries");
    assert_eq!(files.len(), 1);
    let saved = files[0].path();
    assert_eq!(reference.path, saved.to_string_lossy().to_string());
    assert_eq!(
        std::fs::read_to_string(saved).expect("full output"),
        body.display
    );
}
