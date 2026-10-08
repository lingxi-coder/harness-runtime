use super::tests::fixture_runtime;
use super::*;
use crate::headless::stream_json_input::spawn_stdin_router_from_reader;
use std::io::{Cursor, Read};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::mpsc as sync_mpsc;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext, ValidationError,
};
use tool_api::{ToolProgressSender, ToolUseContext};

struct Feed {
    chunk: Cursor<Vec<u8>>,
    incoming: sync_mpsc::UnboundedReceiver<Option<Vec<u8>>>,
}
impl AsyncRead for Feed {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            if self.chunk.position() as usize != self.chunk.get_ref().len() {
                let destination = output.initialize_unfilled();
                let count = self.chunk.read(destination)?;
                output.advance(count);
                return Poll::Ready(Ok(()));
            }
            match self.incoming.poll_recv(cx) {
                Poll::Ready(Some(Some(bytes))) => self.chunk = Cursor::new(bytes),
                Poll::Ready(_) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}
struct CloseFeed(sync_mpsc::UnboundedSender<Option<Vec<u8>>>);
impl Drop for CloseFeed {
    fn drop(&mut self) {
        let _ = self.0.send(None);
    }
}
struct ReplayTool {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    block: bool,
    completed: Arc<std::sync::atomic::AtomicBool>,
}
#[async_trait::async_trait]
impl Tool for ReplayTool {
    fn name(&self) -> &str {
        "OwnedReplay"
    }
    fn input_schema(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| json!({"type":"object","properties":{}}))
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        !self.block
    }
    fn interrupt_behavior(&self, _: &Value) -> InterruptBehavior {
        if self.block {
            InterruptBehavior::Block
        } else {
            InterruptBehavior::Cancel
        }
    }
    async fn validate_input(&self, _: &Value, _: &ToolUseContext) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _: &Value,
        _: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "isolated replay fixture".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: Default::default(),
        }
    }
    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "controlled replay".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        _: Value,
        _: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        self.started.notify_one();
        // Intentionally non-cooperative. The real dispatcher's declared
        // Cancel policy must interrupt it; Block must finish its safe boundary.
        self.release.notified().await;
        self.completed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(ToolCallResult::from_data(json!({"committed":true})))
    }
}

async fn cwd_during_replay(
    runtime: &Runtime,
    plane: &Arc<StdioControlPlane>,
    target: &std::path::Path,
) -> Value {
    let (output, mut response) = tokio::sync::mpsc::unbounded_channel();
    let output = Arc::new(output);
    let writer = ControlPlaneWriter::new(output.clone());
    let lifecycle = crate::headless::queued_commands::QueueLifecycle::new(output, "fixture".into());
    let (cancel, _) = tokio::sync::watch::channel(false);
    // set_cwd compares the confirmation to its resolved directory. On
    // macOS the temporary directory can be reached via /var or /private/var;
    // echo the same canonical path the production trust dialog displays.
    let confirmed = std::fs::canonicalize(target).unwrap();
    let target = confirmed.to_string_lossy();
    let frame = json!({"request":{"path":target,"trust_accepted":true,"trusted_directory":target}});
    dispatch_control_request(
        "set_cwd",
        "cwd",
        &lingxi_core::types::utf16_json::Utf16JsonProjection::plain(frame),
        &writer,
        &cancel,
        &lifecycle,
        &runtime.orchestrator,
        &runtime.task_registry,
        &runtime.session_cwd,
        plane,
        &Arc::new(tokio::sync::Notify::new()),
        &[],
        &[],
        &[],
        &json!({}),
        "off",
        None,
        &StreamFileSuggestionIndex::default(),
        runtime.services.as_ref(),
        &Arc::new(super::lifecycle::PrintAuxTaskGroup::default()),
        &runtime.execution_errors,
    )
    .await;
    let crate::headless::stream_json::OutboundMsg::Line(line) = response.recv().await.unwrap()
    else {
        panic!("control response")
    };
    serde_json::from_str(&line).unwrap()
}

async fn replay_case(block: bool) {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let next = root.path().join("next");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&next).unwrap();
    let stream = Arc::new(StreamJsonStream::new_placeholder(
        crate::headless::io::Output::new(tokio::io::sink()),
    ));
    let mut runtime = fixture_runtime(stream.clone(), root.path()).await;
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut registry = tool_api::registry::ToolRegistry::new();
    registry.register_builtin(Arc::new(ReplayTool {
        started: started.clone(),
        release: release.clone(),
        block,
        completed: completed.clone(),
    }));
    // Real SDK tool dispatcher/history, injected no-provider model and hook
    // collaborators. The CLI owner and its live cwd cell are production ones.
    runtime.inner.orchestrator = Arc::new(
        orchestrator::ConversationOrchestrator::new(
            Default::default(),
            Arc::new(orchestrator::test_support::MockApiClient::new(vec![])),
            Arc::new(registry),
            orchestrator::test_support::noop_hook_executor(),
            Arc::new(orchestrator::test_support::NoOpPermissionGate),
            stream.clone(),
            Arc::new(orchestrator::test_support::StaticMemoryProvider::empty()),
            project.clone(),
        )
        .with_session_cwd(runtime.session_cwd.clone()),
    );
    let id = lingxi_core::types::ToolUseId::new();
    runtime.orchestrator.session().lock().await.history.push(
        lingxi_core::types::ConversationMessage::Assistant { per_turn_effort: None,
            id: lingxi_core::types::MessageId::new(),
            content: vec![lingxi_core::types::ContentBlock::ToolUse {
                input_projection: None,
                id: id.clone(),
                name: "OwnedReplay".into(),
                input: json!({}),
                provider_id: None,
            }],
            stop_reason: None,
        },
    );
    let runtime = Arc::new(runtime);
    let plane = StdioControlPlane::new(stream.outbound_tx());
    let (feed, incoming) = sync_mpsc::unbounded_channel();
    let _close = CloseFeed(feed.clone());
    let replay = json!({"type":"control_response","response":{"subtype":"success","request_id":"lost-fixture","response":{"toolUseID":id,"behavior":"allow"}}});
    feed.send(Some(format!("{replay}\n").into_bytes())).unwrap();
    let lifecycle = Arc::new(crate::headless::queued_commands::QueueLifecycle::new(
        stream.outbound_tx(),
        runtime.orchestrator.session().lock().await.session_id.to_string(),
    ));
    let channels = spawn_stdin_router_from_reader(
        Feed {
            chunk: Cursor::new(Vec::new()),
            incoming,
        },
        crate::headless::io::Output::new(tokio::io::sink()),
        false,
        "fixture".into(),
        stream.outbound_tx(),
        lifecycle.clone(),
    );
    let prepared = super::super::stdio::PreparedStdio::from_channels(
        channels,
        lifecycle,
        plane.clone(),
        runtime.shutdown.child_token(),
    )
    .await;
    let owned_runtime = runtime.clone();
    let owned_plane = plane.clone();
    let tasks = Arc::new(PrintAuxTaskGroup::default());
    let owned_tasks = tasks.clone();
    let mut running = tokio::spawn(async move {
        run_stream_json_input_loop_inner(
            &Argv::default(),
            &owned_runtime,
            stream,
            permission::PermissionMode::Default,
            owned_plane,
            owned_tasks,
            prepared,
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), started.notified())
        .await
        .expect("actual replay tool entered");
    assert!(plane.is_busy().await);
    let cwd_before = runtime.session_cwd.snapshot();
    let moved = cwd_during_replay(&runtime, &plane, &next).await;
    assert_eq!(moved["type"], "control_response", "{moved}");
    assert_eq!(moved["response"]["subtype"], "success", "{moved}");
    assert_eq!(moved["response"]["request_id"], "cwd", "{moved}");
    assert_eq!(
        moved["response"]["response"]["status"], "rejected",
        "{moved}"
    );
    assert_eq!(moved["response"]["response"]["reason"], "busy", "{moved}");
    assert_eq!(
        runtime.session_cwd.snapshot(),
        cwd_before,
        "busy refusal preserves cwd and trusted directories"
    );
    assert_eq!(runtime.session_cwd.cwd(), project);
    feed.send(Some(b"{\"type\":\"control_request\",\"request_id\":\"end\",\"request\":{\"subtype\":\"end_session\"}}\n".to_vec())).unwrap();
    if block {
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut running)
                .await
                .is_err()
        );
        assert!(plane.is_busy().await);
        assert!(plane.try_lock_operation().is_none());
        release.notify_one();
    }
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(3), running)
            .await
            .unwrap()
            .unwrap(),
        exit_codes::SUCCESS
    );
    assert_eq!(completed.load(std::sync::atomic::Ordering::SeqCst), block);
    assert!(!plane.is_busy().await);
    let session = runtime.orchestrator.session();
    let history = &session.lock().await.history;
    assert!(history.iter().any(|message| match message {
        lingxi_core::types::ConversationMessage::User { content, .. } => content.iter().any(|block_result| matches!(block_result,
            lingxi_core::types::ContentBlock::ToolResult { tool_use_id, is_error, .. } if tool_use_id == &id && *is_error == Some(!block))),
        _ => false,
    }), "exact recovered result persisted before owner release");
    tasks.abort_and_join().await;
    assert!(
        runtime
            .session_lifecycle
            .shutdown_and_drain()
            .await
            .complete
    );
}

#[tokio::test]
async fn orphan_cancel_tool_blocks_cwd_and_end_session_returns_with_stdin_open() {
    replay_case(false).await;
}
#[tokio::test]
async fn orphan_block_tool_keeps_cwd_owner_until_its_safe_boundary_completes() {
    replay_case(true).await;
}

#[cfg(unix)]
#[tokio::test]
async fn typed_bash_interrupt_keeps_cwd_owner_until_native_child_is_reaped() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let next = root.path().join("next");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&next).unwrap();
    let stream = Arc::new(StreamJsonStream::new_placeholder(
        crate::headless::io::Output::new(tokio::io::sink()),
    ));
    let runtime = Arc::new(fixture_runtime(stream.clone(), root.path()).await);
    let plane = StdioControlPlane::new(stream.outbound_tx());
    let (feed, incoming) = sync_mpsc::unbounded_channel();
    let _close = CloseFeed(feed.clone());
    let frame =
        json!({"type":"bash_command","command":"printf '%s' \"$$\" > typed.pid; exec sleep 30"});
    feed.send(Some(format!("{frame}\n").into_bytes())).unwrap();
    let lifecycle = Arc::new(crate::headless::queued_commands::QueueLifecycle::new(
        stream.outbound_tx(),
        runtime.orchestrator.session().lock().await.session_id.to_string(),
    ));
    let channels = spawn_stdin_router_from_reader(
        Feed {
            chunk: Cursor::new(Vec::new()),
            incoming,
        },
        crate::headless::io::Output::new(tokio::io::sink()),
        false,
        "fixture".into(),
        stream.outbound_tx(),
        lifecycle.clone(),
    );
    let tasks = Arc::new(PrintAuxTaskGroup::default());
    let prepared = super::super::stdio::PreparedStdio::from_channels(
        channels,
        lifecycle,
        plane.clone(),
        runtime.shutdown.child_token(),
    )
    .await;
    let owned_runtime = runtime.clone();
    let owned_plane = plane.clone();
    let owned_tasks = tasks.clone();
    let running = tokio::spawn(async move {
        run_stream_json_input_loop_inner(
            &Argv::default(),
            &owned_runtime,
            stream,
            permission::PermissionMode::Default,
            owned_plane,
            owned_tasks,
            prepared,
        )
        .await
    });
    let pid: u32 = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(project.join("typed.pid")) {
                if let Ok(pid) = text.parse() {
                    break pid;
                }
            }
            assert!(
                !running.is_finished(),
                "typed shell failed before publishing its PID"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("actual typed shell started");
    assert!(plane.is_busy().await);
    let cwd_before = runtime.session_cwd.snapshot();
    let moved = cwd_during_replay(&runtime, &plane, &next).await;
    assert_eq!(moved["type"], "control_response", "{moved}");
    assert_eq!(moved["response"]["subtype"], "success", "{moved}");
    assert_eq!(moved["response"]["request_id"], "cwd", "{moved}");
    assert_eq!(
        moved["response"]["response"]["status"], "rejected",
        "{moved}"
    );
    assert_eq!(moved["response"]["response"]["reason"], "busy", "{moved}");
    assert_eq!(
        runtime.session_cwd.snapshot(),
        cwd_before,
        "busy refusal preserves cwd and trusted directories"
    );
    assert_eq!(runtime.session_cwd.cwd(), project);
    feed.send(Some(b"{\"type\":\"control_request\",\"request_id\":\"stop\",\"request\":{\"subtype\":\"interrupt\"}}\n".to_vec())).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while plane.is_busy().await {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("typed child joins before the CLI releases admission");
    // Probe only after the actual Bash runner's held child wait/reap returned.
    assert!(!std::process::Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .status()
        .unwrap()
        .success());
    feed.send(Some(b"{\"type\":\"control_request\",\"request_id\":\"end\",\"request\":{\"subtype\":\"end_session\"}}\n".to_vec())).unwrap();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(3), running)
            .await
            .unwrap()
            .unwrap(),
        exit_codes::SUCCESS
    );
    tasks.abort_and_join().await;
    assert!(
        runtime
            .session_lifecycle
            .shutdown_and_drain()
            .await
            .complete
    );
}
