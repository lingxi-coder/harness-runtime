use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
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

    fn result_ids(events: &[platform_api::orchestrator::OutputEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                platform_api::orchestrator::OutputEvent::ToolResult { id, .. } => {
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
        let id = protocol::ToolUseId::new();
        orch.emit_tool_result_frame(&id, "Bash", "out", &serde_json::json!({}), None)
            .await;
        assert_eq!(result_ids(&output.snapshot().await), vec![id.to_string()]);
    }

    /// With buffering ON nothing reaches the stream until release.
    #[tokio::test]
    async fn buffered_frames_are_withheld_then_released_in_caller_order() {
        let output = Arc::new(MockOutputStream::new());
        let orch = orch_for_frames(output.clone());
        orch.set_tool_frame_buffering(true).await;

        let first = protocol::ToolUseId::new();
        let second = protocol::ToolUseId::new();
        // Buffered in COMPLETION order: `second` finished first.
        orch.emit_tool_result_frame(&second, "Bash", "b", &serde_json::json!({}), None)
            .await;
        orch.emit_tool_result_frame(&first, "Read", "a", &serde_json::json!({}), None)
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

        let id = protocol::ToolUseId::new();
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

        let id = protocol::ToolUseId::new();
        orch.emit_tool_result_frame(
            &id,
            "Bash",
            "REAL OUTPUT",
            &serde_json::json!({ "stdout": "REAL OUTPUT" }),
            None,
        )
        .await;
        orch.release_tool_frame(&id, "Bash", "SYNTHETIC", true)
            .await;

        let events = output.snapshot().await;
        let found = events.iter().any(|e| matches!(
            e,
            platform_api::orchestrator::OutputEvent::ToolResult { id: gid, .. } if gid.to_string() == id.to_string()
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

        let id = protocol::ToolUseId::new();
        orch.emit_tool_result_frame(
            &id,
            "Bash",
            "REAL OUTPUT",
            &serde_json::json!({ "error": "aborted" }),
            Some("interrupted"),
        )
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

        let id = protocol::ToolUseId::new();
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
            [platform_api::orchestrator::OutputEvent::ToolResult { id: got_id, tool, .. }]
                if got_id == &id && tool == "McpCancelTool"
        ));
    }

    #[tokio::test]
    async fn no_writer_persist_consumes_frame_side_tables() {
        let output = Arc::new(MockOutputStream::new());
        let orch = orch_for_frames(output);
        orch.set_tool_frame_buffering(true).await;
        let id = protocol::ToolUseId::new();
        orch.record_tool_use_result(&id, serde_json::json!({"ok": true}))
            .await;
        orch.record_tool_denial_kind(&id, "user-rejected").await;
        orch.record_tool_use_mcp_meta(&id, serde_json::json!({"source": "test"}))
            .await;
        orch.record_source_tool_assistant_uuid(&id, "assistant-line".into())
            .await;
        orch.release_tool_frame(&id, "McpTool", "cancelled", true)
            .await;

        let message = protocol::ConversationMessage::User {
            id: protocol::MessageId::new(),
            content: vec![protocol::ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: "cancelled".into(),
                is_error: true,
                provider_tool_use_id: None,
                content_blocks: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        orch.persist_message_to_jsonl(&message).await;

        let key = id.to_string();
        assert!(!orch
            .transcript
            .tool_use_results
            .lock()
            .await
            .contains_key(&key));
        assert!(!orch
            .transcript
            .tool_denial_kinds
            .lock()
            .await
            .contains_key(&key));
        assert!(!orch
            .transcript
            .tool_use_mcp_meta
            .lock()
            .await
            .contains_key(&key));
        assert!(!orch
            .transcript
            .tool_source_assistant_uuids
            .lock()
            .await
            .contains_key(&key));
    }

    #[tokio::test]
    async fn abandoning_a_buffered_frame_consumes_its_side_tables() {
        let orch = orch_for_frames(Arc::new(MockOutputStream::new()));
        orch.set_tool_frame_buffering(true).await;
        let id = protocol::ToolUseId::new();
        orch.emit_tool_result_frame(
            &id,
            "Bash",
            "out",
            &serde_json::json!({"stdout": "out"}),
            Some("interrupted"),
        )
        .await;
        orch.record_tool_use_result(&id, serde_json::json!({"stdout": "out"}))
            .await;
        orch.record_tool_denial_kind(&id, "interrupted").await;

        orch.set_tool_frame_buffering(false).await;

        let key = id.to_string();
        assert!(!orch
            .transcript
            .tool_use_results
            .lock()
            .await
            .contains_key(&key));
        assert!(!orch
            .transcript
            .tool_denial_kinds
            .lock()
            .await
            .contains_key(&key));
    }
}

fn orch_with_writer(dir: &std::path::Path, path: std::path::PathBuf) -> ConversationOrchestrator {
    let fs: Arc<dyn platform_api::FileSystem> = Arc::new(PosixFileSystem::new(dir.to_path_buf()));
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
        "formatted",
        "formatted\n",
        "",
        0,
        "./hooks/fmt.sh",
        37,
    );
    orch.persist_hook_attachment_to_jsonl(payload.clone()).await;

    let raw = std::fs::read_to_string(&path).expect("read jsonl");
    let lines: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "one line: {raw}");
    let v: serde_json::Value = serde_json::from_str(lines[0]).expect("json");
    assert_eq!(v["type"], "attachment");
    assert_eq!(v["attachment"], payload);
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

    let tuid = protocol::ToolUseId::new();
    orch.queue_hook_attachment(
        &tuid,
        hooks::additional_context_attachment(
            "PostToolUse:Edit",
            tuid.as_str(),
            "PostToolUse",
            &["FIRST".to_string()],
        ),
    )
    .await;
    orch.queue_hook_attachment(
        &tuid,
        hooks::error_during_execution_attachment(
            "SECOND",
            "PostToolUse:Edit",
            tuid.as_str(),
            "PostToolUse",
        ),
    )
    .await;

    let msg = ConversationMessage::User {
        id: protocol::MessageId::new(),
        content: vec![protocol::ContentBlock::ToolResult {
            tool_use_id: tuid.clone(),
            content: "ok".into(),
            is_error: false,
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
    impl platform_api::HttpTransport for UnusedHttp {
        async fn request(
            &self,
            _req: protocol::HttpRequest,
        ) -> Result<protocol::HttpResponse, platform_api::HttpError> {
            Err(platform_api::HttpError::InvalidRequest("unused".into()))
        }
        async fn stream_sse(
            &self,
            _req: protocol::HttpRequest,
        ) -> Result<platform_api::http::SseStream, platform_api::HttpError> {
            Err(platform_api::HttpError::InvalidRequest("unused".into()))
        }
    }
    struct UnusedRuntime;
    #[async_trait]
    impl platform_api::RuntimeSpawner for UnusedRuntime {
        async fn spawn(
            &self,
            _name: &str,
            _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<platform_api::BackgroundTaskHandle, platform_api::RuntimeError> {
            Err(platform_api::RuntimeError::Internal("unused".into()))
        }
        async fn sleep(&self, _d: std::time::Duration) {}
        async fn cancel(
            &self,
            _h: &platform_api::BackgroundTaskHandle,
        ) -> Result<(), platform_api::RuntimeError> {
            Ok(())
        }
    }

    let mut registry = HookRegistry::new();
    registry.register(hooks::HookDefinition {
        id: protocol::HookId::new(),
        name: "lint".into(),
        events: vec![hooks::HookEventType::PostToolUse],
        if_condition: None,
        executor: hooks::HookExecutor::Builtin {
            handler_id: "lint".into(),
        },
        source: hooks::HookSource::Settings(protocol::SettingsScope::User),
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
            tool_use_id: protocol::ToolUseId::from("toolu_e2e".to_string()),
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
    sink.record(serde_json::json!({"type": "hook_success"}))
        .await;
    assert!(!path.exists(), "unattached sink writes nothing");

    let orch = Arc::new(orch_with_writer(dir.path(), path.clone()));
    sink.attach(&orch);
    sink.record(serde_json::json!({"type": "hook_cancelled"}))
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
    let body = "x".repeat(hooks::attachment::HOOK_OUTPUT_INLINE_LIMIT + 1);

    let reference = sink
        .persist_large_output(&body)
        .await
        .expect("persisted reference");
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
    assert!(reference.contains(&saved.to_string_lossy().to_string()));
    assert_eq!(std::fs::read_to_string(saved).expect("full output"), body);
}
