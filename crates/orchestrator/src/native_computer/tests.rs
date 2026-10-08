//! Exercise native lowering through the registered dispatcher, hooks, permission
//! gate and durable ordinary transcript. The journal fake records admission
//! facts; session recovery and fsync fault injection live in the session tests.

use super::*;
use crate::conversation::OrchestratorApiClient;
use crate::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use async_trait::async_trait;
use hooks::definition::{HookDefinition, HookExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType};
use hooks::executor::{BuiltinHookHandler, HookExecutorImpl};
use hooks::{HookContext, HookOutcome, HookResponse, HookResult};
use lingxi_core::host::permission_gate::{
    PermissionAbort, PermissionCheckContext, PermissionDecision, PermissionGate, PermissionOutcome,
    PermissionResolution,
};
use lingxi_core::host::{ToolExecutionJournal, ToolJournalAck, ToolJournalError};
use lingxi_core::types::HookId;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex as SyncMutex;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{DescriptionOptions, PromptOptions, ToolStaticContext, ValidationError};

const MODEL: &str = "native-test-model";
const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a4fcAAAAASUVORK5CYII=";

// Focused projection tests enter the same recovery-before-snapshot boundary
// as the real turn driver; production projection itself never repairs history.
async fn prepare_projection(
    orch: &ConversationOrchestrator,
    model: &str,
    profile: Option<&str>,
    tools: &[lingxi_core::types::utf16_json::Utf16JsonProjection],
) -> Result<Option<llm_runtime::computer::ComputerRequestProjection>, OrchestratorError> {
    recover_before_request(orch).await?;
    super::prepare_projection(orch, model, profile, tools).await
}

#[derive(Default)]
struct Journal {
    executions: SyncMutex<HashMap<String, ToolExecutionRecord>>,
    receipts: SyncMutex<HashMap<String, NativeReceiptRecord>>,
    events: SyncMutex<Vec<String>>,
    duplicate_started: AtomicBool,
    reject_received: AtomicBool,
    reject_prepared_after: AtomicUsize,
}

#[async_trait]
impl ToolExecutionJournal for Journal {
    async fn recover_session(
        &self,
        session_id: lingxi_core::types::SessionId,
    ) -> Result<lingxi_core::host::ToolJournalRecovery, ToolJournalError> {
        Ok(lingxi_core::host::ToolJournalRecovery {
            executions: self
                .executions
                .lock()
                .unwrap()
                .values()
                .filter(|record| record.identity.session_id == session_id)
                .cloned()
                .map(ToolExecutionRecord::recovery_view)
                .collect(),
            receipts: self
                .receipts
                .lock()
                .unwrap()
                .values()
                .filter(|record| record.session_id == session_id)
                .cloned()
                .map(NativeReceiptRecord::recovery_view)
                .collect(),
        })
    }
    async fn record_execution(
        &self,
        record: ToolExecutionRecord,
    ) -> Result<ToolJournalAck, ToolJournalError> {
        let event_id = record.event_id();
        let mut events = self.events.lock().unwrap();
        let duplicate = events.contains(&event_id)
            || (record.stage == ToolExecutionStage::Started
                && self.duplicate_started.swap(false, Ordering::SeqCst));
        if !events.contains(&event_id) {
            events.push(event_id.clone());
            self.executions
                .lock()
                .unwrap()
                .insert(record.execution_id(), record);
        }
        Ok(ToolJournalAck {
            event_id,
            journal_revision: events.len() as u64,
            duplicate,
        })
    }
    async fn execution(&self, id: &str) -> Result<Option<ToolExecutionRecord>, ToolJournalError> {
        Ok(self.executions.lock().unwrap().get(id).cloned())
    }
    async fn record_receipt(
        &self,
        record: NativeReceiptRecord,
    ) -> Result<ToolJournalAck, ToolJournalError> {
        if record.stage == NativeReceiptStage::ResponseReceived
            && self.reject_received.swap(false, Ordering::SeqCst)
        {
            return Err(ToolJournalError(
                "crash before Received acknowledgement".into(),
            ));
        }
        if record.stage == NativeReceiptStage::ResponsePrepared
            && self
                .reject_prepared_after
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                == Ok(1)
        {
            return Err(ToolJournalError(
                "crash during response preparation batch".into(),
            ));
        }
        let event_id = record.event_id();
        let mut events = self.events.lock().unwrap();
        let duplicate = events.contains(&event_id);
        if !duplicate {
            events.push(event_id.clone());
            self.receipts
                .lock()
                .unwrap()
                .insert(record.receipt_id.clone(), record);
        }
        Ok(ToolJournalAck {
            event_id,
            journal_revision: events.len() as u64,
            duplicate,
        })
    }
    async fn receipt(&self, id: &str) -> Result<Option<NativeReceiptRecord>, ToolJournalError> {
        Ok(self.receipts.lock().unwrap().get(id).cloned())
    }
}

#[derive(Default)]
struct Gate {
    denied: AtomicBool,
    checks: SyncMutex<Vec<(String, Value)>>,
    resolutions: SyncMutex<Vec<(Option<String>, String, Value)>>,
    prompts: SyncMutex<Vec<(Option<String>, String, Value)>>,
}
#[async_trait]
impl PermissionGate for Gate {
    async fn resolve_detailed_or_abort(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
    ) -> Result<PermissionResolution, PermissionAbort> {
        self.resolutions.lock().unwrap().push((
            ctx.tool_use_id.clone(),
            name.into(),
            input.clone(),
        ));
        Ok(self.resolve_detailed(name, input).await)
    }
    async fn check_with_context(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
    ) -> PermissionOutcome {
        self.prompts
            .lock()
            .unwrap()
            .push((ctx.tool_use_id.clone(), name.into(), input.clone()));
        match self.check(name, input).await {
            PermissionDecision::Allow => PermissionOutcome::Allow {
                updated_input: None,
                permission_updates: vec![],
                decision_classification: None,
            },
            PermissionDecision::Deny { reason } => PermissionOutcome::Deny { reason },
        }
    }
    async fn check(&self, name: &str, input: &Value) -> PermissionDecision {
        self.checks
            .lock()
            .unwrap()
            .push((name.into(), input.clone()));
        PermissionDecision::Allow
    }
    async fn tool_wide_deny_names(&self) -> Vec<String> {
        if self.denied.load(Ordering::SeqCst) {
            vec!["computer".into()]
        } else {
            Vec::new()
        }
    }
}

struct Computer {
    calls: SyncMutex<Vec<Value>>,
    outputs: SyncMutex<Vec<Vec<Value>>>,
    ready: AtomicBool,
    fail_type: bool,
    partial: AtomicBool,
    invalidations: AtomicUsize,
    trace: Arc<SyncMutex<Vec<String>>>,
}
impl Computer {
    fn new(fail_type: bool, trace: Arc<SyncMutex<Vec<String>>>) -> Self {
        Self {
            calls: SyncMutex::new(Vec::new()),
            outputs: SyncMutex::new(Vec::new()),
            ready: AtomicBool::new(true),
            fail_type,
            partial: AtomicBool::new(false),
            invalidations: AtomicUsize::new(0),
            trace,
        }
    }
}
#[async_trait]
impl Tool for Computer {
    fn map_result_text(&self, result: &Value) -> Option<String> {
        tool_api::tool_result_media::computer_batch_model_text(result)
    }
    fn name(&self) -> &str {
        "computer"
    }
    fn input_schema(&self) -> &Value {
        static SCHEMA: once_cell::sync::Lazy<Value> = once_cell::sync::Lazy::new(
            || json!({"type":"object","properties":{"action":{"type":"string"},"geometry_version":{"type":"string"},"text":{"type":"string"}},"required":["action"]}),
        );
        &SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024 * 1024
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        false
    }
    fn is_read_only(&self, _: &Value) -> bool {
        false
    }
    fn native_computer_capabilities(&self) -> Option<ComputerCapabilities> {
        use ComputerOperationKind::*;
        Some(ComputerCapabilities {
            operations: if self.partial.load(Ordering::SeqCst) {
                vec![Screenshot]
            } else {
                vec![
                    Click,
                    Move,
                    Drag,
                    Scroll,
                    ScrollWheel,
                    Key,
                    KeyDown,
                    KeyUp,
                    HoldKey,
                    Type,
                    Wait,
                    Screenshot,
                    Zoom,
                    MouseDown,
                    MouseUp,
                    CursorPosition,
                ]
            },
        })
    }
    fn lower_computer_operation(
        &self,
        op: &ComputerOperation,
        frame: &ComputerFrame,
    ) -> Result<Value, ToolError> {
        if !self
            .native_computer_capabilities()
            .unwrap()
            .supports(op.kind())
        {
            return Err(ToolError::InvalidInput(
                "fixture backend does not support this operation".into(),
            ));
        }
        op.validate(frame)
            .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let point = |p: &ComputerPoint| json!([p.x, p.y]);
        let mut input = match op {
            ComputerOperation::Click {
                button,
                count,
                modifiers,
                ..
            } => json!({
                "action": if *button == ComputerMouseButton::Left && *count == 1 {"left_click"} else {"mouse_click"},
                "button":button,"count":count,"modifiers":modifiers}),
            ComputerOperation::Move {
                point: p,
                modifiers,
            } => json!({"action":"mouse_move","coordinate":point(p),"modifiers":modifiers}),
            ComputerOperation::Drag { path, modifiers } => {
                json!({"action":"left_click_drag","path":path.iter().map(point).collect::<Vec<_>>(),"modifiers":modifiers})
            }
            ComputerOperation::Scroll {
                delta_x,
                delta_y,
                modifiers,
                ..
            } => json!({"action":"scroll","pixel_delta":[delta_x,delta_y],"modifiers":modifiers}),
            ComputerOperation::ScrollWheel {
                direction,
                amount,
                modifiers,
                ..
            } => {
                json!({"action":"scroll","scroll_direction":direction,"scroll_amount":amount,"modifiers":modifiers})
            }
            ComputerOperation::Key { keys } => json!({"action":"key","keys":keys}),
            ComputerOperation::KeyDown { key } => json!({"action":"key_down","text":key}),
            ComputerOperation::KeyUp { key } => json!({"action":"key_up","text":key}),
            ComputerOperation::HoldKey {
                keys,
                duration_seconds,
            } => json!({"action":"hold_key","keys":keys,"duration":duration_seconds}),
            ComputerOperation::Type { text, press_enter } => {
                json!({"action":"type","text":text,"press_enter":press_enter})
            }
            ComputerOperation::Wait { duration_seconds } => {
                json!({"action":"wait","duration":duration_seconds})
            }
            ComputerOperation::Screenshot => json!({"action":"screenshot"}),
            ComputerOperation::Zoom { region } => json!({"action":"zoom","region":region}),
            ComputerOperation::MouseDown { target, modifiers }
            | ComputerOperation::MouseUp { target, modifiers } => {
                let mut input = json!({"action":if matches!(op, ComputerOperation::MouseDown { .. }) {"left_mouse_down"} else {"left_mouse_up"},"modifiers":modifiers});
                if let Some(p) = target {
                    input["coordinate"] = point(p);
                }
                input
            }
            ComputerOperation::CursorPosition => json!({"action":"cursor_position"}),
        };
        if let ComputerOperation::Click { target, .. }
        | ComputerOperation::Scroll { target, .. }
        | ComputerOperation::ScrollWheel { target, .. } = op
        {
            match target {
                ComputerTarget::Position { point: p } => input["coordinate"] = point(p),
                ComputerTarget::CurrentCursor => input["use_current_cursor"] = json!(true),
            }
        }
        input["geometry_version"] = json!(frame.geometry_version);
        Ok(input)
    }
    async fn native_computer_frame(
        &self,
        _: &ToolUseContext,
    ) -> Result<Option<ComputerFrame>, ToolError> {
        Ok(self.ready.load(Ordering::SeqCst).then(|| ComputerFrame {
            width: 1,
            height: 1,
            geometry_version: "frame-1".into(),
        }))
    }
    async fn computer_model_output(
        &self,
        _: &ToolUseContext,
        _: &str,
        blocks: Option<&[Value]>,
    ) -> Result<(), ToolError> {
        let blocks = blocks.unwrap_or_default().to_vec();
        self.ready.store(
            blocks.iter().any(|b| b["type"] == "image"),
            Ordering::SeqCst,
        );
        self.outputs.lock().unwrap().push(blocks);
        Ok(())
    }
    async fn invalidate_computer_observation(&self, _: &ToolUseContext) -> Result<(), ToolError> {
        self.invalidations.fetch_add(1, Ordering::SeqCst);
        self.ready.store(false, Ordering::SeqCst);
        Ok(())
    }
    async fn validate_input(&self, _: &Value, _: &ToolUseContext) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _: &Value,
        _: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Ask {
            reason: permission::PermissionDecisionReason::Other {
                reason: "fixture desktop input".into(),
            },
            prompt: permission::result::PermissionPrompt {
                title: "computer".into(),
                message: "fixture desktop input".into(),
                options: vec![],
            },
            pending_classifier_check: None,
            metadata: Default::default(),
        }
    }
    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "fixture computer".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        input: Value,
        _: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        self.calls.lock().unwrap().push(input.clone());
        self.trace
            .lock()
            .unwrap()
            .push(input["action"].as_str().unwrap().into());
        if self.fail_type && input["action"] == "type" {
            return Err(ToolError::Internal("second input failed".into()));
        }
        let mut result = if input["action"] == "screenshot"
            || input["action"] == "zoom"
            || input["action"] == "request_access"
        {
            ToolCallResult::from_data(
                json!({"type":"image","file":{"base64":PNG,"type":"image/png"}}),
            )
        } else if input["action"] == "computer_batch" {
            ToolCallResult::from_data(json!({"stepsCompleted":1,"results":[{
                "action":"screenshot","result":{"type":"image","file":{"base64":PNG,"type":"image/png"}}
            }]}))
        } else {
            ToolCallResult::from_data(json!({"action":input["action"],"ok":true}))
        };
        result.model_content = self
            .map_result_text(&result.data)
            .or_else(|| Some("fixture result".into()));
        Ok(result)
    }
}

struct Other(&'static str, bool, Arc<SyncMutex<Vec<String>>>);
#[async_trait]
impl Tool for Other {
    fn name(&self) -> &str {
        self.0
    }
    fn input_schema(&self) -> &Value {
        static SCHEMA: once_cell::sync::Lazy<Value> =
            once_cell::sync::Lazy::new(|| json!({"type":"object"}));
        &SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn is_mcp(&self) -> bool {
        self.1
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
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
                reason: "fixture read".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: Default::default(),
        }
    }
    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        "ordinary fixture".into()
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
        self.2.lock().unwrap().push(self.0.into());
        Ok(ToolCallResult::from_data(json!({"content":self.0})))
    }
}

struct Api {
    provider: NativeComputerProvider,
    inner: MockApiClient,
    submission_probe: SyncMutex<Option<Arc<llm_runtime::computer::ComputerReceiptSubmission>>>,
    provider_inputs: SyncMutex<Vec<wire::ChatRequest>>,
}
#[async_trait]
impl OrchestratorApiClient for Api {
    fn native_computer_provider(&self, _: &str, _: Option<&str>) -> Option<NativeComputerProvider> {
        Some(self.provider)
    }
    async fn messages_create(
        &self,
        request: crate::OrchestratorApiRequest,
    ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
        let submission = self.submission_probe.lock().unwrap().clone();
        if let (Some(submission), crate::OrchestratorApiRequest::Main(main)) =
            (submission, &request)
        {
            let messages = llm_runtime::convert::ensure_tool_result_pairing(
                llm_runtime::convert::normalize_messages_for_api(main.messages.clone()),
            );
            let messages = llm_runtime::convert::to_llm_messages(messages)?;
            let tools = llm_runtime::convert::to_tool_declarations(main.tools.clone())?;
            let (input, _) = llm_runtime::convert::history_input(
                &main.model,
                &messages,
                &[],
                &tools,
                protocol(self.provider),
            )?;
            // Capture the SDK projection before admitting the simulated transport.
            self.provider_inputs.lock().unwrap().push(input);
            submission.before_submit().await?;
        }
        self.inner.messages_create(request).await
    }
}

struct RecordingHook {
    events: Arc<SyncMutex<Vec<HookEventType>>>,
    remove_images: bool,
}
#[async_trait]
impl BuiltinHookHandler for RecordingHook {
    fn id(&self) -> &str {
        "native-test-hook"
    }
    async fn handle(&self, event: &HookEvent, _: &HookContext) -> HookResult {
        self.events.lock().unwrap().push(event.event_type());
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
            response: Some(
                if self.remove_images && event.event_type() == HookEventType::PostToolUse {
                    HookResponse {
                        updated_tool_output: Some(Some(json!({"content":"image removed by hook"}))),
                        ..Default::default()
                    }
                } else {
                    HookResponse::default()
                },
            ),
        }
    }
}

struct Fixture {
    orch: Arc<ConversationOrchestrator>,
    api: Arc<Api>,
    computer: Arc<Computer>,
    journal: Arc<Journal>,
    gate: Arc<Gate>,
    hooks: Arc<SyncMutex<Vec<HookEventType>>>,
    transcript_path: std::path::PathBuf,
    trace: Arc<SyncMutex<Vec<String>>>,
    responses: SyncMutex<HashMap<String, llm_runtime::HistoryResponse>>,
    _root: tempfile::TempDir,
}
impl Fixture {
    async fn new(provider: NativeComputerProvider, fail_type: bool, remove_images: bool) -> Self {
        Self::with_responses(provider, fail_type, remove_images, vec![]).await
    }
    async fn with_responses(
        provider: NativeComputerProvider,
        fail_type: bool,
        remove_images: bool,
        responses: Vec<llm_runtime::HistoryResponse>,
    ) -> Self {
        let root = tempfile::tempdir().unwrap();
        let trace = Arc::new(SyncMutex::new(Vec::new()));
        let computer = Arc::new(Computer::new(fail_type, trace.clone()));
        let journal = Arc::new(Journal::default());
        let gate = Arc::new(Gate::default());
        let hooks = Arc::new(SyncMutex::new(Vec::new()));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(computer.clone());
        registry.register_builtin(Arc::new(Other("Read", false, trace.clone())));
        registry.register_mcp_tools(
            lingxi_core::types::McpConnectionId::new(),
            vec![Arc::new(Other(
                "mcp__computer-use__click",
                true,
                trace.clone(),
            ))],
        );
        let mut hook_registry = hooks::HookRegistry::new();
        hook_registry.register(HookDefinition {
            id: HookId::new(),
            name: "native-test-hook".into(),
            events: vec![
                HookEventType::PreToolUse,
                HookEventType::PostToolUse,
                HookEventType::PostToolUseFailure,
            ],
            if_condition: None,
            executor: HookExecutor::Builtin {
                handler_id: "native-test-hook".into(),
            },
            source: HookSource::Session,
            blocking: true,
            timeout: None,
            priority: 0,
            once: false,
            status_message: None,
            async_rewake: false,
            async_timeout: None,
            rewake_message: None,
        });
        let mut executor = HookExecutorImpl::new(
            Arc::new(tokio::sync::RwLock::new(hook_registry)),
            Arc::new(platform_posix::http::PosixHttp::new()),
            Arc::new(platform_posix::runtime::PosixRuntime::new()),
        );
        executor.register_builtin(Arc::new(RecordingHook {
            events: hooks.clone(),
            remove_images,
        }));
        let api = Arc::new(Api {
            provider,
            inner: MockApiClient::new(responses),
            submission_probe: SyncMutex::new(None),
            provider_inputs: SyncMutex::new(Vec::new()),
        });
        let orch = ConversationOrchestrator::new(
            OrchestratorConfig {
                model: MODEL.into(),
                interactive_permissions: true,
                ..Default::default()
            },
            api.clone(),
            Arc::new(registry),
            Arc::new(executor),
            gate.clone(),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            root.path().to_path_buf(),
        )
        .with_config_home(root.path().to_path_buf())
        .with_tool_execution_journal(journal.clone())
        .with_verified_computer_profiles(vec![VerifiedComputerProfile {
            model: MODEL.into(),
            profile: None,
            provider,
            evidence: "fixture acceptance only".into(),
        }]);
        let session_id = orch.session.lock().await.session_id;
        let state_root = root.path().join("durable-state");
        std::fs::create_dir_all(&state_root).unwrap();
        let transcript_path = session::jsonl::session_path(
            root.path(),
            &root.path().to_string_lossy(),
            &session_id.as_uuid().to_string(),
        );
        let writer = Arc::new(
            session::jsonl::JsonlWriter::new(
                transcript_path.clone(),
                Arc::new(platform_posix::fs::PosixFileSystem::new(
                    root.path().to_path_buf(),
                )),
            )
            .with_durable_lock(Arc::new(
                session::jsonl::DurableTranscriptWriter::open(&state_root).unwrap(),
            )),
        );
        writer
            .activate_session_target(
                session_id,
                transcript_path.clone(),
                root.path().to_path_buf(),
            )
            .unwrap();
        let orch =
            orch.with_jsonl_writer(writer)
                .with_hook_registry(Arc::new(tokio::sync::RwLock::new(
                    hooks::HookRegistry::new(),
                )));
        let orch = ConversationOrchestrator::into_shared(orch);
        Self {
            orch,
            api,
            computer,
            journal,
            gate,
            hooks,
            transcript_path,
            trace,
            responses: SyncMutex::new(HashMap::new()),
            _root: root,
        }
    }
    fn fence(&self) -> crate::autonomous_tool_scheduler::ToolDispatchPublicationFence {
        let (cancel, lock) = self
            .orch
            .lifecycle_runtime
            .session_tool_hook_generation
            .current();
        crate::autonomous_tool_scheduler::ToolDispatchPublicationFence::new(cancel, lock)
    }
    async fn native(&self) {
        let (tools, _) = self.orch.build_wire_tools().await;
        prepare_projection(&self.orch, MODEL, None, &tools)
            .await
            .unwrap();
        let ctx = crate::turn_loop::streaming_tool_context_base(&self.orch, vec![]).await;
        let tool: Arc<dyn Tool> = self.computer.clone();
        note_computer_result(&self.orch, &tool, &ctx, true).await;
        assert!(prepare_projection(&self.orch, MODEL, None, &tools)
            .await
            .unwrap()
            .unwrap()
            .native
            .is_some());
    }
    async fn bind(
        &self,
        response: &llm_runtime::HistoryResponse,
    ) -> Result<Vec<ContentBlock>, OrchestratorError> {
        let blocks = bind_response(
            &self.orch,
            response,
            crate::turn_loop::translate_response_blocks(&response.content),
        )
        .await?;
        self.responses
            .lock()
            .unwrap()
            .insert(response.id.clone(), response.clone());
        Ok(blocks)
    }
    async fn dispatch(&self, blocks: Vec<ContentBlock>) -> crate::turn_loop::DeferredToolDispatch {
        self.dispatch_checked(blocks).await.unwrap()
    }
    async fn accept_response(
        &self,
        response: &llm_runtime::HistoryResponse,
    ) -> Result<(), OrchestratorError> {
        let blocks = self.bind(response).await?;
        let message = ConversationMessage::Assistant { per_turn_effort: None,
            id: MessageId::new(),
            content: blocks,
            stop_reason: response.stop_reason.clone(),
        };
        let accepted =
            persist_native_assistant(&self.orch, &message, response, None, self.fence()).await?;
        self.orch.session.lock().await.history.push(accepted);
        Ok(())
    }
    async fn dispatch_checked(
        &self,
        blocks: Vec<ContentBlock>,
    ) -> Result<crate::turn_loop::DeferredToolDispatch, OrchestratorError> {
        let response_id = blocks.iter().find_map(|block| match block {
            ContentBlock::ProviderContent { value, .. }
                if value["type"] == "lingxi_computer_binding" =>
            {
                value["provider_response_id"].as_str().map(str::to_owned)
            }
            _ => None,
        });
        let existing = if let Some(response_id) = &response_id {
            self.orch.session.lock().await.history.iter().find(|message| matches!(message,
                ConversationMessage::Assistant { content, .. } if content.iter().any(|block| matches!(block,
                    ContentBlock::ProviderContent { value, .. } if value["type"] == "lingxi_computer_binding" && value["provider_response_id"] == *response_id))))
                .cloned()
        } else {
            None
        };
        let mut assistant_message =
            existing
                .clone()
                .unwrap_or_else(|| ConversationMessage::Assistant { per_turn_effort: None,
                    id: MessageId::new(),
                    content: blocks.clone(),
                    stop_reason: Some("tool_use".into()),
                });
        let assistant_id = assistant_message.id();
        if existing.is_none() {
            if let Some(response_id) = response_id {
                let response = self
                    .responses
                    .lock()
                    .unwrap()
                    .get(&response_id)
                    .unwrap()
                    .clone();
                let fence = self.fence();
                assistant_message = persist_native_assistant(
                    &self.orch,
                    &assistant_message,
                    &response,
                    None,
                    fence,
                )
                .await?;
            } else {
                let row = prepare_stored_row(&self.orch, &assistant_message).await?;
                persist_stored_row(
                    &self.orch,
                    &row,
                    &format!("fixture-assistant:{assistant_id}"),
                )
                .await?;
            }
            self.orch
                .session
                .lock()
                .await
                .history
                .push(assistant_message.clone());
        }
        let uses = blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse {
                    id,
                    name,
                    input,
                    provider_id,
                 .. } => Some((id.clone(), name.clone(), input.clone(), provider_id.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut dispatched = dispatch_tools(
            &self.orch,
            &uses,
            assistant_id,
            crate::turn_loop::ToolUseDispatchFacts {
                query_history: vec![],
                assistant_message,
                same_turn_tool_uses: vec![],
            },
            self.fence(),
        )
        .await?;
        // The ordinary turn driver owns this publication boundary for every
        // accepted dispatch, before individual native result rows are saved.
        assert!(dispatched.publish_results(&self.orch, &self.fence()).await);
        Ok(dispatched)
    }
    async fn prepared_receipt(&self) -> (Vec<ContentBlock>, NativeReceiptRecord) {
        self.native().await;
        let blocks = self
            .bind(&openai(json!([{"type":"screenshot"}])))
            .await
            .unwrap();
        let dispatched = self.dispatch(blocks.clone()).await;
        self.publish(&dispatched.results).await;
        prepare_receipts(&self.orch, &result_ids(&dispatched.results))
            .await
            .unwrap();
        let receipt = self
            .journal
            .receipts
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .clone();
        (blocks, receipt)
    }
    async fn forget_runtime(&self) {
        // Simulate the fresh runtime owner reconstructed by the host from its
        // existing durable history and authoritative journal recovery view.
        *self.orch.computer_runtime.state.lock().await = RoundState::default();
        self.orch.computer_runtime.work.lock().await.clear();
    }
    async fn publish(&self, blocks: &[ContentBlock]) {
        // Match visible_response's per-result transcript topology, including
        // sibling media blocks owned by the immediately preceding result.
        let mut messages: Vec<Vec<ContentBlock>> = Vec::new();
        for block in blocks {
            if matches!(block, ContentBlock::ToolResult { .. }) || messages.is_empty() {
                messages.push(vec![block.clone()]);
            } else {
                messages.last_mut().unwrap().push(block.clone());
            }
        }
        for content in messages {
            final_model_result(&self.orch, &content).await.unwrap();
            let message = ConversationMessage::User { api_message_override: None,
                id: MessageId::new(),
                content,
                is_meta: false,
                is_compact_summary: false,
                is_visible_in_transcript_only: false,
            };
            assert!(publish_native_result(&self.orch, &message).await.unwrap());
            self.orch.session.lock().await.history.push(message);
        }
    }
}

fn response(
    provider: NativeComputerProvider,
    content: Vec<llm_runtime::ContentBlock>,
) -> llm_runtime::HistoryResponse {
    let mut response = mock_message_response(content, Some("tool_use"));
    response.id = "native-response-1".into();
    response.model = MODEL.into();
    response.provider_metadata = json!({"llm_client":{"computer_binding":{"account":"fixture-account","profile":"fixture-profile","model":MODEL,"endpoint":"fixture-endpoint","protocol":protocol_name(provider)}}});
    if provider == NativeComputerProvider::OpenAi {
        response.provider_metadata["llm_client"]["continuation"] = json!({
            "response_id":response.id,"protocol":protocol_name(provider),"provider_id":"openai",
            "profile_name":"fixture-profile","endpoint_fingerprint":"fixture-endpoint",
            "account_scope":"fixture-account","request_model":MODEL,
        });
    }
    response
}

fn openai(actions: Value) -> llm_runtime::HistoryResponse {
    use lingxi_llm_client::providers::openai::computer::OpenAiComputerCall;
    let call: OpenAiComputerCall = serde_json::from_value(json!({"type":"computer_call","id":"item-1","call_id":"call-1","pending_safety_checks":[],"status":"completed","actions":actions})).unwrap();
    let block = wire::ContentBlock::Native {
        value: wire::NativeExtension::from_typed(call).unwrap(),
    };
    response(
        NativeComputerProvider::OpenAi,
        vec![llm_runtime::ContentBlock::ProviderContent {
            protocol: protocol_name(NativeComputerProvider::OpenAi),
            value: json!({"type":"lingxi_native_content","block":block}),
        }],
    )
}

fn claude_screenshot() -> llm_runtime::HistoryResponse {
    let block: wire::ContentBlock = serde_json::from_value(json!({"type":"tool_use","id":"call-1","name":"screenshot","input":{},"toolset_name":"computer"})).unwrap();
    response(
        NativeComputerProvider::Anthropic,
        vec![
            llm_runtime::ContentBlock::ToolCall { input_projection: None,
                id: "call-1".into(),
                name: "screenshot".into(),
                input: json!({}),
            },
            llm_runtime::ContentBlock::ProviderContent {
                protocol: protocol_name(NativeComputerProvider::Anthropic),
                value: json!({"type":"lingxi_replay_metadata","block":block}),
            },
        ],
    )
}

fn result_ids(blocks: &[ContentBlock]) -> Vec<ToolUseId> {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn ordinary_batch_images_use_the_existing_turn_driver_and_final_hook_output() {
    for remove_images in [false, true] {
        let request = response(
            NativeComputerProvider::OpenAi,
            vec![llm_runtime::ContentBlock::ToolCall { input_projection: None,
                id: "batch-1".into(),
                name: "computer".into(),
                input: json!({"action":"computer_batch","actions":[{"action":"screenshot"}]}),
            }],
        );
        let fixture = Fixture::with_responses(
            NativeComputerProvider::OpenAi,
            false,
            remove_images,
            vec![request],
        )
        .await;
        crate::turn_loop::execute_one_turn(&fixture.orch, None)
            .await
            .unwrap();
        let history = fixture.orch.session.lock().await.history.clone();
        let (text, blocks) = history
            .iter()
            .flat_map(|message| match message {
                ConversationMessage::User { content, .. } => content.as_slice(),
                _ => &[],
            })
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    content_blocks,
                    ..
                } if tool_use_id.as_str() == "batch-1" => {
                    Some((content, content_blocks.as_deref().unwrap_or_default()))
                }
                _ => None,
            })
            .expect("ordinary driver publishes the batch result");
        assert!(!text.contains(PNG));
        assert_eq!(
            blocks
                .iter()
                .filter(|block| block["type"] == "image")
                .count(),
            usize::from(!remove_images)
        );
        assert_eq!(
            fixture.computer.ready.load(Ordering::SeqCst),
            !remove_images
        );
        assert_eq!(
            *fixture.hooks.lock().unwrap(),
            [HookEventType::PreToolUse, HookEventType::PostToolUse]
        );
        assert_eq!(
            fixture.computer.calls.lock().unwrap().len(),
            1,
            "wrapper keeps one ordinary lifecycle"
        );
        assert!(fixture.journal.executions.lock().unwrap().is_empty());
        assert!(fixture.journal.receipts.lock().unwrap().is_empty());
        let rows: Vec<Value> = std::fs::read_to_string(&fixture.transcript_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            rows.iter()
                .any(|row| row["toolUseResult"]["results"][0]["result"]["file"]["base64"] == PNG),
            "ordinary audit retains the raw wrapper, including after hook removal"
        );
    }
}

#[tokio::test]
async fn ordered_native_failure_stops_input_and_publishes_ordinary_results() {
    let fixture = Fixture::new(NativeComputerProvider::OpenAi, true, false).await;
    fixture.native().await;
    let response = openai(
        json!([{"type":"click","x":0,"y":0,"button":"left"},{"type":"type","text":"fails"},{"type":"screenshot"}]),
    );
    let blocks = fixture.bind(&response).await.unwrap();
    let dispatched = fixture.dispatch(blocks).await;
    let ids = result_ids(&dispatched.results);
    assert_eq!(ids.len(), 3);
    assert_eq!(
        fixture
            .computer
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|i| i["action"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["left_click", "type"]
    );
    // Policy resolution and the tool's explicit Ask each consult the gate.
    // Attribute both stages to actual member IDs, excluding catalog probes.
    for checks in [&fixture.gate.resolutions, &fixture.gate.prompts] {
        let checks = checks.lock().unwrap();
        let admitted = checks
            .iter()
            .filter(|(id, ..)| id.is_some())
            .collect::<Vec<_>>();
        assert_eq!(admitted.len(), 2);
        for ((id, name, input), expected) in admitted.into_iter().zip(&ids[..2]) {
            assert_eq!(id.as_deref(), Some(expected.as_str()));
            assert_eq!(name, "computer");
            assert!(matches!(
                input["action"].as_str(),
                Some("left_click" | "type")
            ));
        }
    }
    assert_eq!(
        *fixture.hooks.lock().unwrap(),
        [
            HookEventType::PreToolUse,
            HookEventType::PostToolUse,
            HookEventType::PreToolUse,
            HookEventType::PostToolUseFailure
        ]
    );
    fixture.publish(&dispatched.results).await;
    let work = fixture.orch.computer_runtime.work.lock().await;
    for (id, expected) in ids.iter().zip([
        ToolExecutionOutcome::Succeeded,
        ToolExecutionOutcome::Failed,
        ToolExecutionOutcome::Skipped,
    ]) {
        let record = fixture
            .journal
            .execution(&work[id].identity.execution_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.stage, ToolExecutionStage::OutputPublished);
        assert_eq!(record.outcome, Some(expected));
    }
    drop(work);
    let transcript = std::fs::read_to_string(&fixture.transcript_path).unwrap();
    for id in ids {
        assert!(
            transcript.contains(id.as_str()),
            "ordinary result missing from durable transcript"
        );
    }
}

#[tokio::test]
async fn mod_success_replacement_cannot_admit_input_after_durable_failure() {
    let fixture = Fixture::new(NativeComputerProvider::OpenAi, true, false).await;
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    let module = fixture._root.path().join("replace-failure.mjs");
    std::fs::write(
        &module,
        r#"
        export function register(on) {
          on('tool.call', {tool:'computer'}, async ($, event, next) => {
            const answer = await next(event);
            return {...answer, result:{ok:true}, isError:false};
          });
        }
    "#,
    )
    .unwrap();
    host.load("replace-failure", fixture._root.path(), &module, json!({}))
        .await
        .unwrap();
    fixture
        .orch
        .lifecycle_runtime
        .hook_registry
        .as_ref()
        .unwrap()
        .write()
        .await
        .set_mod_host(host);
    fixture.native().await;
    let blocks = fixture.bind(&openai(json!([
        {"type":"type","text":"fails"}, {"type":"click","x":0,"y":0,"button":"left"}, {"type":"screenshot"}
    ]))).await.unwrap();
    let dispatched = fixture.dispatch(blocks).await;
    assert_eq!(fixture.computer.calls.lock().unwrap().len(), 1);
    assert!(matches!(
        &dispatched.results[0],
        ContentBlock::ToolResult {
            is_error: Some(false),
            ..
        }
    ));
    let records = fixture.journal.executions.lock().unwrap();
    assert!(records
        .values()
        .any(|record| record.identity.member_index == 0
            && record.outcome == Some(ToolExecutionOutcome::Failed)));
    assert_eq!(
        records
            .values()
            .filter(|record| record.outcome == Some(ToolExecutionOutcome::Skipped))
            .count(),
        2
    );
}

#[tokio::test]
async fn recovering_existing_prepared_receipt_preserves_a_later_transcript_tip() {
    let fixture = Fixture::new(NativeComputerProvider::OpenAi, false, false).await;
    fixture.prepared_receipt().await;
    let message = ConversationMessage::user(MessageId::new(), "Continue after restart".into());
    let row = prepare_stored_row(&fixture.orch, &message).await.unwrap();
    persist_stored_row(&fixture.orch, &row, "later-user")
        .await
        .unwrap();
    fixture.orch.session.lock().await.history.push(message);
    fixture.forget_runtime().await;
    let (catalog, _) = fixture.orch.build_wire_tools().await;
    prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap();
    assert_eq!(
        *fixture.orch.transcript.last_jsonl_uuid.lock().await,
        Some(row.uuid.clone())
    );
    let next = prepare_stored_row(
        &fixture.orch,
        &ConversationMessage::user(MessageId::new(), "next".into()),
    )
    .await
    .unwrap();
    assert_eq!(next.parent_uuid, Some(row.uuid));
}

#[tokio::test]
async fn compacted_completed_receipt_cannot_block_a_reused_call_in_a_new_response() {
    let fixture = Fixture::new(NativeComputerProvider::OpenAi, false, false).await;
    fixture.prepared_receipt().await;
    let (catalog, _) = fixture.orch.build_wire_tools().await;
    prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap()
        .submission
        .unwrap()
        .before_submit()
        .await
        .unwrap();
    let mut successor = response(NativeComputerProvider::OpenAi, vec![]);
    successor.id = "old-successor".into();
    successor.provider_metadata["llm_client"]["continuation"]["response_id"] = json!(successor.id);
    successor.stop_reason = Some("end_turn".into());
    fixture.accept_response(&successor).await.unwrap();
    fixture.orch.session.lock().await.history.clear();
    fixture.native().await;
    let mut new = openai(json!([{"type":"screenshot"}]));
    new.id = "new-response".into();
    new.provider_metadata["llm_client"]["continuation"]["response_id"] = json!(new.id);
    let blocks = fixture.bind(&new).await.unwrap();
    let results = fixture.dispatch(blocks).await;
    fixture.publish(&results.results).await;
    fixture.forget_runtime().await;
    let recovered = prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap();
    let receipts = recovered.submission.unwrap().receipts.clone();
    assert_eq!(receipts.len(), 1);
    let record = fixture
        .journal
        .execution(&receipts[0].execution_ids[0])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.identity.provider_response_id, "new-response");
    assert_eq!(fixture.computer.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn confirmed_gemini_output_recovers_without_any_in_memory_safety_acknowledgement() {
    let fixture = Fixture::new(NativeComputerProvider::Gemini, false, false).await;
    fixture.native().await;
    let check = json!({"decision":"require_confirmation","explanation":"submit form"});
    let block = wire::ContentBlock::Native {value:wire::NativeExtension::new(
        lingxi_llm_client::providers::google::computer::CALL_FORMAT,
        json!({"type":"function_call","id":"call-1","name":"click","arguments":{"x":0,"y":0,"safety_decision":check}}),
    ).unwrap()};
    let response = response(
        NativeComputerProvider::Gemini,
        vec![llm_runtime::ContentBlock::ProviderContent {
            protocol: protocol_name(NativeComputerProvider::Gemini),
            value: json!({"type":"lingxi_native_content","block":block}),
        }],
    );
    let blocks = fixture.bind(&response).await.unwrap();
    let results = fixture.dispatch(blocks).await;
    fixture.publish(&results.results).await;
    assert!(fixture.journal.receipts.lock().unwrap().is_empty());
    assert!(fixture
        .journal
        .executions
        .lock()
        .unwrap()
        .values()
        .any(|record| record.acknowledged_safety_checks == vec![check.clone()]));
    fixture.forget_runtime().await;
    let (catalog, _) = fixture.orch.build_wire_tools().await;
    let recovered = prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recovered.submission.unwrap().receipts.len(), 1);
    assert_eq!(fixture.computer.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn confirmed_but_failed_gemini_input_recovers_as_a_failed_receipt() {
    let fixture = Fixture::new(NativeComputerProvider::Gemini, true, false).await;
    fixture.native().await;
    let check = json!({"decision":"require_confirmation","explanation":"submit form"});
    let block = wire::ContentBlock::Native {value:wire::NativeExtension::new(
        lingxi_llm_client::providers::google::computer::CALL_FORMAT,
        json!({"type":"function_call","id":"call-1","name":"type","arguments":{"text":"fails","safety_decision":check}}),
    ).unwrap()};
    let response = response(
        NativeComputerProvider::Gemini,
        vec![llm_runtime::ContentBlock::ProviderContent {
            protocol: protocol_name(NativeComputerProvider::Gemini),
            value: json!({"type":"lingxi_native_content","block":block}),
        }],
    );
    let blocks = fixture.bind(&response).await.unwrap();
    let results = fixture.dispatch(blocks).await;
    fixture.publish(&results.results).await;
    assert!(fixture
        .journal
        .executions
        .lock()
        .unwrap()
        .values()
        .any(
            |record| record.outcome == Some(ToolExecutionOutcome::Failed)
                && record.acknowledged_safety_checks == vec![check.clone()]
        ));
    fixture.forget_runtime().await;
    let (catalog, _) = fixture.orch.build_wire_tools().await;
    let recovered = prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        recovered.submission.unwrap().receipts[0].stage,
        NativeReceiptStage::Prepared
    );
    assert_eq!(fixture.computer.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn duplicate_started_admission_and_recovered_started_never_replay_input() {
    let fixture = Fixture::new(NativeComputerProvider::OpenAi, false, false).await;
    fixture.native().await;
    let blocks = fixture
        .bind(&openai(
            json!([{"type":"click","x":0,"y":0,"button":"left"}]),
        ))
        .await
        .unwrap();
    fixture
        .journal
        .duplicate_started
        .store(true, Ordering::SeqCst);
    let first = fixture.dispatch(blocks.clone()).await;
    fixture.publish(&first.results).await;
    let started = fixture
        .journal
        .executions
        .lock()
        .unwrap()
        .values()
        .find(|record| record.identity.member_index == 0)
        .unwrap()
        .clone();
    assert_eq!(
        started.stage,
        ToolExecutionStage::OutputPublished,
        "a duplicate Started ACK publishes an observation-required result"
    );
    assert_eq!(started.outcome, Some(ToolExecutionOutcome::Unknown));
    assert_eq!(
        fixture
            .journal
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| **event == format!("{}:Started", started.execution_id()))
            .count(),
        1
    );
    assert!(fixture
        .journal
        .events
        .lock()
        .unwrap()
        .contains(&format!("{}:OutcomeUnknown", started.execution_id())));
    assert!(fixture
        .journal
        .executions
        .lock()
        .unwrap()
        .values()
        .any(|record| record.identity.member_index == 1
            && record.outcome == Some(ToolExecutionOutcome::Skipped)));
    assert!(fixture.computer.calls.lock().unwrap().is_empty());
    assert!(first.results.iter().any(|b| matches!(
        b,
        ContentBlock::ToolResult {
            is_error: Some(true),
            ..
        }
    )));
    fixture
        .journal
        .duplicate_started
        .store(false, Ordering::SeqCst);
    let second = fixture.dispatch(blocks).await;
    assert!(fixture.computer.calls.lock().unwrap().is_empty());
    assert_eq!(
        second.results, first.results,
        "recovery must reuse the published unknown result"
    );
    assert_eq!(
        fixture.gate.prompts.lock().unwrap().len(),
        1,
        "recovered Started must bypass even fresh permission admission"
    );
}

#[tokio::test]
async fn live_unfinished_native_outputs_abandon_the_old_call_without_replaying_input() {
    for provider in [
        NativeComputerProvider::OpenAi,
        NativeComputerProvider::Anthropic,
        NativeComputerProvider::Gemini,
    ] {
        for started in [false, true] {
            let fixture = Fixture::new(provider, false, false).await;
            fixture.native().await;
            let origin = match provider {
                NativeComputerProvider::OpenAi => {
                    openai(json!([{"type":"click","x":0,"y":0,"button":"left"}]))
                }
                NativeComputerProvider::Anthropic => claude_screenshot(),
                NativeComputerProvider::Gemini => response(
                    provider,
                    vec![llm_runtime::ContentBlock::ProviderContent {
                        protocol: protocol_name(provider),
                        value: json!({"type":"lingxi_native_content","block":wire::ContentBlock::Native {
                            value: wire::NativeExtension::new(
                                lingxi_llm_client::providers::google::computer::CALL_FORMAT,
                                json!({"type":"function_call","id":"call-1","name":"click","arguments":{"x":0,"y":0}}),
                            ).unwrap(),
                        }}),
                    }],
                ),
            };
            if started {
                let blocks = fixture.bind(&origin).await.unwrap();
                fixture.dispatch(blocks).await;
                // The backend and hooks finished, but storing the final output
                // failed before any OutputPrepared acknowledgement.
                assert!(fixture
                    .journal
                    .executions
                    .lock()
                    .unwrap()
                    .values()
                    .all(|r| { r.stage == ToolExecutionStage::Terminal && r.output.is_none() }));
            } else {
                // The accepted assistant was published, then the turn stopped
                // before its first Started acknowledgement.
                fixture.accept_response(&origin).await.unwrap();
            }
            let calls = fixture.computer.calls.lock().unwrap().len();
            let hooks = fixture.hooks.lock().unwrap().len();
            let facts = fixture.journal.executions.lock().unwrap().clone();
            let (catalog, _) = fixture.orch.build_wire_tools().await;
            let next = prepare_projection(&fixture.orch, MODEL, None, &catalog)
                .await
                .unwrap()
                .unwrap();
            assert!(
                next.continuation.is_none(),
                "unfinished {provider:?} call cannot reuse its server boundary"
            );
            assert!(next.native.is_none());
            assert!(next.submission.is_none());
            let history = fixture.orch.session.lock().await.history.clone();
            assert!(history.iter().flat_map(|message| match message {
                ConversationMessage::User { content, .. } | ConversationMessage::Assistant { content, .. } => content.as_slice(),
                ConversationMessage::System { .. } => &[],
            }).any(|b| matches!(b,
                ContentBlock::ProviderContent { value, .. } if value["type"] == "lingxi_computer_abandoned")));
            let invalidations = fixture.computer.invalidations.load(Ordering::SeqCst);
            assert_eq!(invalidations, 1);
            let messages = llm_runtime::convert::to_llm_messages(
                llm_runtime::convert::ensure_tool_result_pairing(
                    llm_runtime::convert::normalize_messages_for_api(history),
                ),
            )
            .unwrap();
            let tools = llm_runtime::convert::to_tool_declarations(catalog.clone()).unwrap();
            let (input, _) = llm_runtime::computer::scope_computer_request(Some(next), async {
                llm_runtime::convert::history_input(
                    MODEL,
                    &messages,
                    &[],
                    &tools,
                    protocol(provider),
                )
            })
            .await
            .unwrap();
            let input_json = serde_json::to_string(&input).unwrap();
            assert!(!input_json.contains("computer_call"));
            assert!(!input_json.contains("toolset_name"));
            assert!(input_json.contains("fresh observation"));
            assert_eq!(*fixture.journal.executions.lock().unwrap(), facts);
            assert_eq!(fixture.computer.calls.lock().unwrap().len(), calls);
            assert_eq!(fixture.hooks.lock().unwrap().len(), hooks);
            assert!(fixture.journal.receipts.lock().unwrap().is_empty());

            // A fresh ordinary observation must remain usable on later turns;
            // old unfinished audit facts cannot continually revoke it.
            let observed = fixture
                .dispatch(vec![ContentBlock::ToolUse { input_projection: None,
                    id: ToolUseId::new(),
                    name: "computer".into(),
                    input: json!({"action":"screenshot"}),
                    provider_id: None,
                }])
                .await;
            final_model_result(&fixture.orch, &observed.results)
                .await
                .unwrap();
            let fresh = prepare_projection(&fixture.orch, MODEL, None, &catalog)
                .await
                .unwrap()
                .unwrap();
            assert!(fresh.native.is_some());
            assert_eq!(
                fixture.computer.invalidations.load(Ordering::SeqCst),
                invalidations
            );
        }
    }
}

#[tokio::test]
async fn dynamic_scope_denial_removes_declaration_and_rejects_inflight_native_work() {
    let fixture = Fixture::new(NativeComputerProvider::Anthropic, false, false).await;
    fixture.native().await;
    fixture.gate.denied.store(true, Ordering::SeqCst);
    let error = fixture.bind(&claude_screenshot()).await.unwrap_err();
    assert!(error.to_string().contains("scope"));
    let (catalog, _) = fixture.orch.build_wire_tools().await;
    assert!(!catalog.iter().any(|tool| tool["name"] == "computer"));
    assert!(catalog.iter().any(|tool| tool["name"] == "Read"));
    assert!(catalog
        .iter()
        .any(|tool| tool["name"] == "mcp__computer-use__click"));
    assert!(prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .is_none());
    let forged = fixture.bind(&claude_screenshot()).await.unwrap_err();
    assert!(forged.to_string().contains("without a scoped declaration"));
    assert!(fixture.computer.calls.lock().unwrap().is_empty());
    assert!(fixture.journal.executions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn native_members_keep_their_position_among_ordinary_calls() {
    let fixture = Fixture::new(NativeComputerProvider::Anthropic, false, false).await;
    fixture.native().await;
    let mut response = claude_screenshot();
    response.content.insert(
        0,
        llm_runtime::ContentBlock::ToolCall { input_projection: None,
            id: "click-1".into(),
            name: "left_click".into(),
            input: json!({"coordinate":[0,0]}),
        },
    );
    response.content.insert(
        1,
        llm_runtime::ContentBlock::ProviderContent {
            protocol: protocol_name(NativeComputerProvider::Anthropic),
            value: json!({"type":"lingxi_replay_metadata","block":{
                "type":"tool_use","id":"click-1","name":"left_click",
                "input":{"coordinate":[0,0]},"toolset_name":"computer"
            }}),
        },
    );
    response.content.insert(
        2,
        serde_json::from_value(json!({
            "type":"tool_call","id":"read-1","name":"Read","input":{}
        }))
        .unwrap(),
    );
    let blocks = fixture.bind(&response).await.unwrap();
    let declared = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { name, input, .. } => Some(if name == "computer" {
                input["action"].as_str().unwrap().to_owned()
            } else {
                name.clone()
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(declared, ["left_click", "Read", "screenshot"]);
    let dispatched = fixture.dispatch(blocks).await;
    assert_eq!(result_ids(&dispatched.results).len(), 3);
    assert_eq!(
        *fixture.trace.lock().unwrap(),
        ["left_click", "Read", "screenshot"]
    );
}

#[tokio::test]
async fn native_assistant_durable_write_failure_prevents_all_input_admission() {
    let fixture = Fixture::new(NativeComputerProvider::OpenAi, false, false).await;
    fixture.native().await;
    let blocks = fixture
        .bind(&openai(json!([
            {"type":"click","x":0,"y":0,"button":"left"},
            {"type":"screenshot"}
        ])))
        .await
        .unwrap();
    // A directory at the canonical JSONL target causes the real durable
    // writer to fail its append before dispatcher admission can start.
    std::fs::create_dir_all(&fixture.transcript_path).unwrap();
    let error = fixture
        .dispatch_checked(blocks)
        .await
        .err()
        .expect("durable assistant append must fail");
    assert!(error.to_string().contains("native computer"));
    assert!(fixture.computer.calls.lock().unwrap().is_empty());
    assert!(fixture.gate.checks.lock().unwrap().is_empty());
    assert!(fixture.hooks.lock().unwrap().is_empty());
    assert!(fixture.journal.executions.lock().unwrap().is_empty());
    assert!(fixture.journal.receipts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn post_hook_removed_image_is_absent_from_model_frame_and_receipt() {
    let fixture = Fixture::new(NativeComputerProvider::Anthropic, false, true).await;
    fixture.native().await;
    let blocks = fixture.bind(&claude_screenshot()).await.unwrap();
    let dispatched = fixture.dispatch(blocks).await;
    assert!(!serde_json::to_string(&dispatched.results)
        .unwrap()
        .contains(PNG));
    fixture.publish(&dispatched.results).await;
    assert!(!fixture.computer.ready.load(Ordering::SeqCst));
    assert!(fixture
        .computer
        .outputs
        .lock()
        .unwrap()
        .iter()
        .all(|blocks| !blocks.iter().any(|b| b["type"] == "image")));
    // Claude's required observation must fail explicitly, and cannot obtain
    // the hidden original image or perform another screenshot behind the hook.
    let receipt = prepare_receipts(&fixture.orch, &result_ids(&dispatched.results)).await;
    assert!(receipt.is_err());
    assert_eq!(fixture.computer.calls.lock().unwrap().len(), 1);
    assert_eq!(fixture.journal.receipts.lock().unwrap().len(), 1);
    assert!(fixture
        .journal
        .receipts
        .lock()
        .unwrap()
        .values()
        .all(|record| record.stage == NativeReceiptStage::CannotResume));
    let rows = std::fs::read_to_string(&fixture.transcript_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(
        rows.iter()
            .any(|row| serde_json::to_string(&row["toolUseResult"])
                .unwrap()
                .contains(PNG)),
        "the existing raw-result audit metadata retains the executed screenshot"
    );
    for row in rows.iter().filter(|row| row["type"] == "user") {
        assert!(!serde_json::to_string(&row["message"]["content"])
            .unwrap()
            .contains(PNG));
    }
    let history = fixture.orch.session.lock().await.history.clone();
    let messages = llm_runtime::convert::to_llm_messages(history).unwrap();
    let (provider_input, _) = llm_runtime::convert::history_input(
        MODEL,
        &messages,
        &[],
        &[],
        wire::ProtocolFamily::AnthropicMessages,
    )
    .unwrap();
    assert!(
        !serde_json::to_string(&provider_input)
            .unwrap()
            .contains(PNG),
        "provider-bound history must not read the raw audit image"
    );
    assert!(
        !serde_json::to_string(&*fixture.journal.receipts.lock().unwrap())
            .unwrap()
            .contains(PNG)
    );
}

#[tokio::test]
async fn function_native_function_switch_uses_registered_catalog_and_final_history() {
    let fixture = Fixture::new(NativeComputerProvider::OpenAi, false, false).await;
    let (catalog, _) = fixture.orch.build_wire_tools().await;
    assert!(prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap()
        .native
        .is_none());
    let function = fixture
        .dispatch(vec![ContentBlock::ToolUse { input_projection: None,
            id: ToolUseId::new(),
            name: "computer".into(),
            input: json!({"action":"request_access"}),
            provider_id: None,
        }])
        .await;
    final_model_result(&fixture.orch, &function.results)
        .await
        .unwrap();
    assert!(prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap()
        .native
        .is_some());
    let mut blocks = fixture
        .bind(&openai(json!([{"type":"screenshot"}])))
        .await
        .unwrap();
    blocks.extend(
        ["Read", "mcp__computer-use__click"].map(|name| ContentBlock::ToolUse { input_projection: None,
            id: ToolUseId::new(),
            name: name.into(),
            input: json!({}),
            provider_id: None,
        }),
    );
    let dispatched = fixture.dispatch(blocks).await;
    assert_eq!(result_ids(&dispatched.results).len(), 3);
    let native_ids = fixture
        .orch
        .computer_runtime
        .work
        .lock()
        .await
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    let native_results = dispatched.results.iter().filter(|b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if native_ids.contains(tool_use_id))).cloned().collect::<Vec<_>>();
    fixture.publish(&native_results).await;
    prepare_receipts(&fixture.orch, &native_ids).await.unwrap();
    let (next_catalog, _) = fixture.orch.build_wire_tools().await;
    let next = prepare_projection(&fixture.orch, MODEL, None, &next_catalog)
        .await
        .unwrap()
        .unwrap();
    assert!(next.native.is_none());
    assert_eq!(
        catalog
            .iter()
            .map(|t| t["name"].clone())
            .collect::<Vec<_>>(),
        next_catalog
            .iter()
            .map(|t| t["name"].clone())
            .collect::<Vec<_>>()
    );
    next.submission.unwrap().before_submit().await.unwrap();
    let mut final_response = mock_message_response(vec![], Some("end_turn"));
    final_response.id = "final-response".into();
    final_response.model = MODEL.into();
    fixture.accept_response(&final_response).await.unwrap();
    assert!(fixture
        .journal
        .receipts
        .lock()
        .unwrap()
        .values()
        .all(|r| r.stage == NativeReceiptStage::ResponseReceived));
    assert_eq!(
        fixture
            .computer
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|i| i["action"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["request_access", "screenshot"]
    );
    let history = fixture.orch.session.lock().await.history.clone();
    assert!(history.iter().any(|message| matches!(message, ConversationMessage::Assistant { content, .. } if content.iter().any(|b| matches!(b, ContentBlock::ToolUse { id, name, .. } if native_ids.contains(id) && name == "computer")))));
    assert!(history.iter().any(|message| matches!(message, ConversationMessage::User { content, .. } if content.iter().any(|b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if native_ids.contains(tool_use_id))))));
}

#[tokio::test]
async fn partial_backend_keeps_working_function_tools_without_native_declaration() {
    let fixture = Fixture::new(NativeComputerProvider::OpenAi, false, false).await;
    fixture.computer.partial.store(true, Ordering::SeqCst);
    let (catalog, _) = fixture.orch.build_wire_tools().await;
    prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap();
    let dispatched = fixture
        .dispatch(vec![ContentBlock::ToolUse { input_projection: None,
            id: ToolUseId::new(),
            name: "computer".into(),
            input: json!({"action":"screenshot"}),
            provider_id: None,
        }])
        .await;
    final_model_result(&fixture.orch, &dispatched.results)
        .await
        .unwrap();
    assert!(fixture.computer.ready.load(Ordering::SeqCst));
    let next = prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap();
    assert!(
        next.native.is_none(),
        "Responses cannot disable unsupported backend actions"
    );
    assert!(fixture
        .orch
        .computer_projection_reason()
        .await
        .unwrap()
        .contains("capability"));
    assert_eq!(fixture.computer.calls.lock().unwrap().len(), 1);
    assert!(catalog.iter().any(|tool| tool["name"] == "computer"));
    assert!(fixture.journal.executions.lock().unwrap().is_empty());
    assert!(fixture
        .bind(&openai(json!([{"type":"screenshot"}])))
        .await
        .is_err());
}

#[tokio::test]
async fn fresh_owner_reuses_prepared_receipt_and_published_output_without_input() {
    let fixture = Fixture::new(NativeComputerProvider::OpenAi, false, false).await;
    let (blocks, saved) = fixture.prepared_receipt().await;
    fixture.forget_runtime().await;
    let (catalog, _) = fixture.orch.build_wire_tools().await;
    let projection = prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap();
    assert!(projection.native.is_none());
    let submission = projection.submission.expect("recover prepared receipt");
    assert_eq!(submission.receipts.len(), 1);
    assert_eq!(
        submission.receipts[0], saved,
        "receipt identity and media bytes must survive owner reconstruction"
    );
    let replayed = fixture.dispatch(blocks).await;
    assert_eq!(result_ids(&replayed.results).len(), 1);
    assert_eq!(fixture.computer.calls.lock().unwrap().len(), 1);
    submission.before_submit().await.unwrap();
    let mut final_response = mock_message_response(vec![], Some("end_turn"));
    final_response.model = MODEL.into();
    fixture.accept_response(&final_response).await.unwrap();
    assert_eq!(
        fixture
            .journal
            .receipt(&saved.receipt_id)
            .await
            .unwrap()
            .unwrap()
            .stage,
        NativeReceiptStage::ResponseReceived
    );
}

#[tokio::test]
async fn fresh_owner_refuses_uncertain_submitted_receipt_without_input_or_resubmission() {
    let fixture = Fixture::new(NativeComputerProvider::OpenAi, false, false).await;
    let (_, saved) = fixture.prepared_receipt().await;
    let (catalog, _) = fixture.orch.build_wire_tools().await;
    let projection = prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap();
    projection
        .submission
        .unwrap()
        .before_submit()
        .await
        .unwrap();
    fixture.forget_runtime().await;
    let error = prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("outcome is unknown"));
    assert_eq!(fixture.computer.calls.lock().unwrap().len(), 1);
    assert_eq!(
        fixture
            .journal
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.as_str()
                == format!("native-receipt:{}:1:Submitted", saved.receipt_id))
            .count(),
        1
    );
}

#[tokio::test]
async fn completed_native_rows_stay_compacted_when_preparing_the_next_request() {
    let mut fixture = Fixture::new(NativeComputerProvider::OpenAi, false, false).await;
    let mut orch = Arc::try_unwrap(fixture.orch)
        .ok()
        .expect("fixture is the sole owner before compactor setup");
    orch.streaming_tool_dispatch_owner.take();
    fixture.orch =
        ConversationOrchestrator::into_shared(crate::test_support::with_scripted_compactor(
            orch,
            "The desktop task completed. Continue the ordinary conversation.",
        ));
    fixture
        .orch
        .session
        .lock()
        .await
        .history
        .push(ConversationMessage::user(
            MessageId::new(),
            "Observe the desktop once.".into(),
        ));
    let (_, saved) = fixture.prepared_receipt().await;
    let (catalog, _) = fixture.orch.build_wire_tools().await;
    prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap()
        .submission
        .unwrap()
        .before_submit()
        .await
        .unwrap();
    let mut response = mock_message_response(vec![], Some("end_turn"));
    response.id = "native-acknowledgement".into();
    response.model = MODEL.into();
    fixture.accept_response(&response).await.unwrap();
    assert_eq!(
        fixture
            .journal
            .receipt(&saved.receipt_id)
            .await
            .unwrap()
            .unwrap()
            .stage,
        NativeReceiptStage::ResponseReceived
    );

    // Keep ordinary recent rounds so the real forced-compaction pipeline
    // summarizes the completed native round rather than preserving it as tail.
    for index in 0..6 {
        fixture
            .orch
            .session
            .lock()
            .await
            .history
            .push(ConversationMessage::user(
                MessageId::new(),
                format!("Ordinary follow-up {index}"),
            ));
        fixture
            .dispatch(vec![ContentBlock::Text {
                text: format!("Ordinary answer {index}"),
                citations: None,
            }])
            .await;
    }
    fixture
        .orch
        .force_compact_with_cancel(CancellationToken::new())
        .await
        .unwrap();
    let compacted = fixture.orch.session.lock().await.history.clone();
    let compacted_json = serde_json::to_value(&compacted).unwrap();
    let compacted_text = serde_json::to_string(&compacted_json).unwrap();
    for marker in [
        "lingxi_computer_binding",
        "lingxi_computer_receipt",
        "native_computer_",
    ] {
        assert!(
            !compacted_text.contains(marker),
            "compaction must remove completed native rows"
        );
    }
    let transcript_before = std::fs::read(&fixture.transcript_path).unwrap();
    let (catalog, _) = fixture.orch.build_wire_tools().await;
    let next = prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap();
    assert!(next.submission.is_none());
    assert_eq!(serde_json::to_value(&fixture.orch.session.lock().await.history).unwrap(), compacted_json,
        "routine ledger refresh must not resurrect completed binding, output, receipt or acknowledgement rows");
    assert_eq!(
        std::fs::read(&fixture.transcript_path).unwrap(),
        transcript_before
    );
    assert_eq!(fixture.computer.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn fresh_owner_does_not_append_completed_rows_after_a_retained_successor_boundary() {
    let fixture = Fixture::new(NativeComputerProvider::OpenAi, false, false).await;
    let (_, saved) = fixture.prepared_receipt().await;
    let (catalog, _) = fixture.orch.build_wire_tools().await;
    prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap()
        .submission
        .unwrap()
        .before_submit()
        .await
        .unwrap();
    let mut successor = response(NativeComputerProvider::OpenAi, vec![]);
    successor.id = "retained-successor".into();
    successor.provider_metadata["llm_client"]["continuation"]["response_id"] = json!(successor.id);
    successor.stop_reason = Some("end_turn".into());
    fixture.accept_response(&successor).await.unwrap();
    assert_eq!(
        fixture
            .journal
            .receipt(&saved.receipt_id)
            .await
            .unwrap()
            .unwrap()
            .stage,
        NativeReceiptStage::ResponseReceived
    );
    fixture.orch.session.lock().await.history.retain(|message| !matches!(message,
        ConversationMessage::User { content, .. } if content.iter().any(|block| matches!(block,
            ContentBlock::ToolResult { .. }) || matches!(block,
            ContentBlock::ProviderContent { value, .. } if value["type"] == "lingxi_computer_receipt"))));
    let retained = serde_json::to_value(&fixture.orch.session.lock().await.history).unwrap();
    let transcript_before = std::fs::read(&fixture.transcript_path).unwrap();
    fixture.forget_runtime().await;
    let next = prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap();
    assert!(next.submission.is_none());
    assert_eq!(
        next.continuation.unwrap().response_id.as_str(),
        "retained-successor"
    );
    assert_eq!(serde_json::to_value(&fixture.orch.session.lock().await.history).unwrap(), retained,
        "received ledger facts must not add old output, receipt or a derived ACK after the successor");
    assert_eq!(
        std::fs::read(&fixture.transcript_path).unwrap(),
        transcript_before
    );
    assert_eq!(fixture.computer.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn old_unknown_call_id_does_not_invalidate_a_new_successful_response_scope() {
    let fixture = Fixture::new(NativeComputerProvider::OpenAi, false, false).await;
    fixture.native().await;
    let mut old = openai(json!([{"type":"screenshot"}]));
    old.id = "unknown-response".into();
    old.provider_metadata["llm_client"]["continuation"]["response_id"] = json!(old.id);
    let old_blocks = fixture.bind(&old).await.unwrap();
    fixture
        .journal
        .duplicate_started
        .store(true, Ordering::SeqCst);
    let old_results = fixture.dispatch(old_blocks).await;
    fixture.publish(&old_results.results).await;
    prepare_receipts(&fixture.orch, &result_ids(&old_results.results))
        .await
        .unwrap();
    let (catalog, _) = fixture.orch.build_wire_tools().await;
    assert!(prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap()
        .native
        .is_none());

    let observed = fixture
        .dispatch(vec![ContentBlock::ToolUse { input_projection: None,
            id: ToolUseId::new(),
            name: "computer".into(),
            input: json!({"action":"screenshot"}),
            provider_id: None,
        }])
        .await;
    final_model_result(&fixture.orch, &observed.results)
        .await
        .unwrap();
    assert!(prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap()
        .native
        .is_some());
    let mut new = openai(json!([{"type":"screenshot"}]));
    new.id = "successful-response".into();
    new.provider_metadata["llm_client"]["continuation"]["response_id"] = json!(new.id);
    let new_blocks = fixture.bind(&new).await.unwrap();
    let new_results = fixture.dispatch(new_blocks).await;
    fixture.publish(&new_results.results).await;
    // Lose the runtime before Prepared, while an older Unknown reused this ID.
    fixture.forget_runtime().await;
    let invalidations = fixture.computer.invalidations.load(Ordering::SeqCst);
    assert!(fixture.computer.ready.load(Ordering::SeqCst));
    let next = prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        next.continuation.unwrap().response_id.as_str(),
        "successful-response"
    );
    assert!(next.submission.is_some());
    assert_eq!(
        fixture.computer.invalidations.load(Ordering::SeqCst),
        invalidations
    );
    assert!(fixture.computer.ready.load(Ordering::SeqCst));
    assert_eq!(
        fixture.computer.calls.lock().unwrap().len(),
        2,
        "unknown input stays unexecuted while both fresh observations run once"
    );
    let messages =
        llm_runtime::convert::to_llm_messages(fixture.orch.session.lock().await.history.clone())
            .unwrap();
    let (input, _) = llm_runtime::convert::history_input(
        MODEL,
        &messages,
        &[],
        &[],
        wire::ProtocolFamily::OpenAiResponses,
    )
    .unwrap();
    assert!(serde_json::to_string(&input).unwrap().contains("openai.responses.computer_call_output.v1"),
        "an old abandonment marker must not suppress the new response's receipt with a reused call ID");
}

#[tokio::test]
async fn compacted_unknown_input_uses_its_durable_binding_without_replaying_or_restoring_old_output(
) {
    let fixture = Fixture::new(NativeComputerProvider::OpenAi, false, false).await;
    fixture.native().await;
    let blocks = fixture
        .bind(&openai(
            json!([{"type":"click","x":0,"y":0,"button":"left"}]),
        ))
        .await
        .unwrap();
    fixture
        .journal
        .duplicate_started
        .store(true, Ordering::SeqCst);
    let results = fixture.dispatch(blocks).await;
    fixture.publish(&results.results).await;
    prepare_receipts(&fixture.orch, &result_ids(&results.results))
        .await
        .unwrap();
    assert!(fixture.journal.receipts.lock().unwrap().is_empty());
    fixture.orch.session.lock().await.history = vec![ConversationMessage::user(
        MessageId::new(),
        "Compacted context: input outcome unknown, observe again.".into(),
    )];
    fixture.forget_runtime().await;
    let (catalog, _) = fixture.orch.build_wire_tools().await;
    let next = prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap();
    assert!(next.submission.is_none());
    assert!(next.continuation.is_none());
    assert!(fixture.computer.calls.lock().unwrap().is_empty());
    assert!(!fixture.orch.session.lock().await.history.iter().any(|message| matches!(message,
        ConversationMessage::User { content, .. } if content.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. }))
    )));
    let observed = fixture
        .dispatch(vec![ContentBlock::ToolUse { input_projection: None,
            id: ToolUseId::new(),
            name: "computer".into(),
            input: json!({"action":"screenshot"}),
            provider_id: None,
        }])
        .await;
    final_model_result(&fixture.orch, &observed.results)
        .await
        .unwrap();
    assert!(prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap()
        .native
        .is_some());
    assert_eq!(fixture.computer.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn saved_complete_successor_repairs_received_acknowledgement_without_resubmission() {
    let fixture = Fixture::new(NativeComputerProvider::OpenAi, false, false).await;
    let (_, receipt) = fixture.prepared_receipt().await;
    let (catalog, _) = fixture.orch.build_wire_tools().await;
    prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap()
        .submission
        .unwrap()
        .before_submit()
        .await
        .unwrap();
    let mut successor = response(NativeComputerProvider::OpenAi, vec![]);
    successor.id = "durable-successor".into();
    successor
        .content
        .push(llm_runtime::ContentBlock::TextJsUtf16 {
            text: "\u{fffd}".into(),
            utf16_code_units: vec![0xd800],
            citations: None,
            cache_control: None,
        });
    successor.provider_metadata["llm_client"]["continuation"]["response_id"] = json!(successor.id);
    fixture
        .journal
        .reject_received
        .store(true, Ordering::SeqCst);
    assert!(fixture.accept_response(&successor).await.is_err());
    assert_eq!(
        fixture
            .journal
            .receipt(&receipt.receipt_id)
            .await
            .unwrap()
            .unwrap()
            .stage,
        NativeReceiptStage::ResponsePrepared
    );
    let bytes_before = std::fs::read(&fixture.transcript_path).unwrap();
    assert!(
        String::from_utf8_lossy(&bytes_before).contains("\\ud800"),
        "accepted successor preserves exact JS string units"
    );
    fixture.forget_runtime().await;
    let next = prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap();
    assert!(next.submission.is_none());
    assert_eq!(
        next.continuation.unwrap().response_id.as_str(),
        "durable-successor"
    );
    assert_eq!(
        fixture
            .journal
            .receipt(&receipt.receipt_id)
            .await
            .unwrap()
            .unwrap()
            .stage,
        NativeReceiptStage::ResponseReceived
    );
    assert_eq!(
        std::fs::read(&fixture.transcript_path).unwrap(),
        bytes_before,
        "the full row was already durable; recovery must not append it twice"
    );
    assert_eq!(fixture.computer.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn altered_response_attempt_is_rejected_before_repairing_history() {
    let fixture = Fixture::new(NativeComputerProvider::OpenAi, false, false).await;
    let (_, receipt) = fixture.prepared_receipt().await;
    let (catalog, _) = fixture.orch.build_wire_tools().await;
    prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap()
        .submission
        .unwrap()
        .before_submit()
        .await
        .unwrap();
    let mut successor = response(NativeComputerProvider::OpenAi, vec![]);
    successor.id = "attempt-successor".into();
    successor.provider_metadata["llm_client"]["continuation"]["response_id"] = json!(successor.id);
    fixture
        .journal
        .reject_received
        .store(true, Ordering::SeqCst);
    assert!(fixture.accept_response(&successor).await.is_err());
    fixture
        .journal
        .receipts
        .lock()
        .unwrap()
        .get_mut(&receipt.receipt_id)
        .unwrap()
        .submission_attempt += 1;
    fixture.forget_runtime().await;
    assert!(prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .is_err());
    assert_eq!(fixture.computer.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn partially_prepared_response_batch_repairs_all_receipts_from_one_complete_successor() {
    use lingxi_llm_client::providers::openai::computer::OpenAiComputerCall;
    let fixture = Fixture::new(NativeComputerProvider::OpenAi, false, false).await;
    fixture.native().await;
    let mut origin = openai(json!([{"type":"screenshot"}]));
    let call: OpenAiComputerCall = serde_json::from_value(json!({"type":"computer_call","id":"item-2","call_id":"call-2","pending_safety_checks":[],"status":"completed","actions":[{"type":"screenshot"}]})).unwrap();
    origin.content.push(llm_runtime::ContentBlock::ProviderContent {
        protocol: protocol_name(NativeComputerProvider::OpenAi),
        value: json!({"type":"lingxi_native_content","block":wire::ContentBlock::Native { value:wire::NativeExtension::from_typed(call).unwrap() }}),
    });
    let blocks = fixture.bind(&origin).await.unwrap();
    let dispatched = fixture.dispatch(blocks).await;
    fixture.publish(&dispatched.results).await;
    prepare_receipts(&fixture.orch, &result_ids(&dispatched.results))
        .await
        .unwrap();
    let (catalog, _) = fixture.orch.build_wire_tools().await;
    let submission = prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap()
        .submission
        .unwrap();
    assert_eq!(submission.receipts.len(), 2);
    submission.before_submit().await.unwrap();
    let mut successor = response(NativeComputerProvider::OpenAi, vec![]);
    successor.id = "complete-batch-successor".into();
    successor.provider_metadata["llm_client"]["continuation"]["response_id"] = json!(successor.id);
    fixture
        .journal
        .reject_prepared_after
        .store(2, Ordering::SeqCst);
    assert!(fixture.accept_response(&successor).await.is_err());
    let records = fixture
        .journal
        .receipts
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        records
            .iter()
            .filter(|r| r.stage == NativeReceiptStage::ResponsePrepared)
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|r| r.stage == NativeReceiptStage::Submitted)
            .count(),
        1
    );
    let bytes_before = std::fs::read(&fixture.transcript_path).unwrap();
    fixture.forget_runtime().await;
    let recovered = prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap();
    assert!(recovered.submission.is_none());
    assert_eq!(
        recovered.continuation.unwrap().response_id.as_str(),
        "complete-batch-successor"
    );
    assert!(fixture
        .journal
        .receipts
        .lock()
        .unwrap()
        .values()
        .all(|r| r.stage == NativeReceiptStage::ResponseReceived));
    let bytes_after = std::fs::read(&fixture.transcript_path).unwrap();
    assert!(
        bytes_after.len() > bytes_before.len(),
        "the complete saved successor had not reached the transcript before the crash"
    );
    fixture.forget_runtime().await;
    prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(&fixture.transcript_path).unwrap(),
        bytes_after
    );
    assert_eq!(fixture.computer.calls.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn recovered_receipt_is_in_the_actual_turn_request_before_submission() {
    for fresh in [false, true] {
        let mut successor = mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "done".into(),
                citations: None,
                cache_control: None,
            }],
            Some("end_turn"),
        );
        successor.id = "after-repaired-receipt".into();
        successor.model = MODEL.into();
        let fixture = Fixture::with_responses(
            NativeComputerProvider::OpenAi,
            false,
            false,
            vec![successor],
        )
        .await;
        fixture
            .orch
            .session
            .lock()
            .await
            .history
            .push(ConversationMessage::user(
                MessageId::new(),
                "take a screenshot".into(),
            ));
        let (_, saved) = fixture.prepared_receipt().await;
        fixture.orch.session.lock().await.history.retain(|message| {
            let content = match message {
                ConversationMessage::User { content, .. }
                | ConversationMessage::Assistant { content, .. } => content.as_slice(),
                ConversationMessage::System { .. } => &[],
            };
            !content.iter().any(|block| {
                matches!(block,
            ContentBlock::ProviderContent {value,..} if value["type"]=="lingxi_computer_receipt")
            })
        });
        fixture
            .orch
            .session
            .lock()
            .await
            .history
            .push(ConversationMessage::user(
                MessageId::new(),
                "continue".into(),
            ));
        if fresh {
            fixture.forget_runtime().await;
        }
        *fixture.api.submission_probe.lock().unwrap() =
            Some(Arc::new(llm_runtime::computer::ComputerReceiptSubmission {
                journal: fixture.journal.clone(),
                receipts: vec![saved.clone()],
            }));
        let result = crate::turn_loop::execute_one_turn(&fixture.orch, None)
            .await
            .unwrap();
        assert!(matches!(
            result,
            crate::turn_loop::TurnStepOutcome::Ended { .. }
        ));
        let inputs = fixture.api.provider_inputs.lock().unwrap();
        assert_eq!(
            inputs.len(),
            1,
            "actual API input missing; last message={:?}",
            fixture.orch.session.lock().await.history.last()
        );
        assert!(inputs[0].messages.iter().flat_map(|message|&message.content).any(|block|matches!(block,
            wire::ContentBlock::Native {value} if value.format()==lingxi_llm_client::providers::openai::computer::OPENAI_COMPUTER_CALL_OUTPUT_FORMAT && value.data()["call_id"]=="call-1")),
            "the actual SDK input must contain the recovered receipt, fresh={fresh}");
        drop(inputs);
        assert_eq!(
            fixture
                .journal
                .receipt(&saved.receipt_id)
                .await
                .unwrap()
                .unwrap()
                .stage,
            NativeReceiptStage::ResponseReceived
        );
        assert_eq!(
            fixture.computer.calls.lock().unwrap().len(),
            1,
            "recovery must never repeat desktop input"
        );
    }
}

#[tokio::test]
async fn gemini_member_name_collision_falls_back_with_the_actual_registry_catalog() {
    let fixture = Fixture::new(NativeComputerProvider::Gemini, false, false).await;
    fixture.native().await;
    fixture.orch.tools.register_mod_tool(
        "collision-fixture",
        Arc::new(Other("click", false, fixture.trace.clone())),
    );
    let (catalog, _) = fixture.orch.build_wire_tools().await;
    assert!(catalog.iter().any(|tool| tool["name"] == "click"));
    let projection = prepare_projection(&fixture.orch, MODEL, None, &catalog)
        .await
        .unwrap()
        .unwrap();
    assert!(projection.native.is_none());
    assert!(fixture
        .orch
        .computer_projection_reason()
        .await
        .unwrap()
        .contains("cannot coexist"));
    let declarations = llm_runtime::convert::to_tool_declarations(catalog).unwrap();
    let (input, _) = llm_runtime::convert::history_input(
        MODEL,
        &[],
        &[],
        &declarations,
        protocol(NativeComputerProvider::Gemini),
    )
    .unwrap();
    assert!(input.tools.iter().any(|tool| tool.name == "computer"));
    assert!(input.tools.iter().any(|tool| tool.name == "click"));
    assert!(fixture.journal.executions.lock().unwrap().is_empty());
    assert!(fixture.computer.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn full_compaction_preserves_unsubmitted_native_round_and_receipt_for_fresh_owners() {
    for provider in [
        NativeComputerProvider::OpenAi,
        NativeComputerProvider::Gemini,
    ] {
        for unsent_stage in [
            NativeReceiptStage::Prepared,
            NativeReceiptStage::NotSubmitted,
        ] {
            for fresh in [false, true] {
                let mut successor = mock_message_response(
                    vec![llm_runtime::ContentBlock::Text {
                        text: "done".into(),
                        citations: None,
                        cache_control: None,
                    }],
                    Some("end_turn"),
                );
                successor.id = "after-compaction".into();
                successor.model = MODEL.into();
                let fixture =
                    Fixture::with_responses(provider, false, false, vec![successor]).await;
                fixture
                    .orch
                    .session
                    .lock()
                    .await
                    .history
                    .push(ConversationMessage::user(
                        MessageId::new(),
                        "old context to summarize".into(),
                    ));
                fixture.native().await;
                let response = match provider {
                    NativeComputerProvider::OpenAi => openai(json!([{"type":"screenshot"}])),
                    NativeComputerProvider::Gemini => {
                        let block = wire::ContentBlock::Native { value: wire::NativeExtension::new(
                        lingxi_llm_client::providers::google::computer::CALL_FORMAT,
                        json!({"type":"function_call","id":"call-1","name":"take_screenshot","arguments":{}}),
                    ).unwrap() };
                        response(
                            provider,
                            vec![llm_runtime::ContentBlock::ProviderContent {
                                protocol: protocol_name(provider),
                                value: json!({"type":"lingxi_native_content","block":block}),
                            }],
                        )
                    }
                    _ => unreachable!(),
                };
                let blocks = fixture.bind(&response).await.unwrap();
                let dispatched = fixture.dispatch(blocks).await;
                fixture.publish(&dispatched.results).await;
                prepare_receipts(&fixture.orch, &result_ids(&dispatched.results))
                    .await
                    .unwrap();
                let mut saved = fixture
                    .journal
                    .receipts
                    .lock()
                    .unwrap()
                    .values()
                    .next()
                    .unwrap()
                    .clone();
                if unsent_stage == NativeReceiptStage::NotSubmitted {
                    saved.stage = NativeReceiptStage::Submitted;
                    saved.submission_attempt = 1;
                    fixture.journal.record_receipt(saved.clone()).await.unwrap();
                    saved.stage = NativeReceiptStage::NotSubmitted;
                    fixture.journal.record_receipt(saved.clone()).await.unwrap();
                }
                let origin_id = fixture.orch.session.lock().await.history.iter().find(|message| matches!(message,
                ConversationMessage::Assistant {content,..} if content.iter().any(|block| matches!(block,
                    ContentBlock::ProviderContent {value,..} if value["type"]=="lingxi_computer_binding")))).unwrap().id();
                let later = ConversationMessage::user(
                    MessageId::new(),
                    "continue after the receipt".into(),
                );
                let later_id = later.id();
                fixture.orch.session.lock().await.history.push(later);
                let exact = ConversationMessage::User { api_message_override: None,
                    id: MessageId::new(),
                    is_meta: false,
                    is_compact_summary: false,
                    is_visible_in_transcript_only: false,
                    content: vec![ContentBlock::TextJsUtf16 {
                        text: "\u{fffd}".into(),
                        utf16_code_units: vec![0xd800],
                        citations: None,
                    }],
                };
                let exact_id = exact.id();
                fixture.orch.persist_message_to_jsonl(&exact).await;
                fixture.orch.session.lock().await.history.push(exact);
                let derived = ConversationMessage::User { api_message_override: None,
                    id: MessageId::new(),
                    is_meta: true,
                    is_compact_summary: false,
                    is_visible_in_transcript_only: false,
                    content: vec![ContentBlock::ProviderContent {
                        protocol: protocol_name(provider),
                        value: json!({"type":"lingxi_computer_abandoned",
                        "call_ids":["old-call"], "call_identities":[["old-response","old-call"]],
                        "reason":"old execution outcome is unknown; observe instead of repeating input"}),
                    }],
                };
                let derived_id = derived.id();

                // Simulate identity registration succeeding before a physical
                // JSONL append fails: the sidecar has the UUID but no row.
                let row = prepare_stored_row(&fixture.orch, &derived).await.unwrap();
                let writer = fixture.orch.transcript.jsonl_writer.as_ref().unwrap();
                let session_id = fixture.orch.session.lock().await.session_id;
                writer
                    .append_json_once_durable_for_session_with_tip_exact(
                        session_id,
                        &format!("fixture-identity-only:{}", row.uuid),
                        serde_json::to_value(&row).unwrap(),
                        session::jsonl::exact_json::message_utf16_overrides(&row),
                    )
                    .await
                    .unwrap();
                let bytes = std::fs::read_to_string(&fixture.transcript_path).unwrap();
                let filtered: String = bytes
                    .split_inclusive('\n')
                    .filter(|line| {
                        serde_json::from_str::<Value>(line)
                            .ok()
                            .is_none_or(|value| value["uuid"] != row.uuid)
                    })
                    .collect();
                std::fs::write(&fixture.transcript_path, filtered).unwrap();
                let identity_path = std::path::PathBuf::from(format!(
                    "{}{}",
                    fixture.transcript_path.display(),
                    branding::SESSION_MESSAGE_IDENTITY_LOG_SUFFIX
                ));
                assert!(
                    std::fs::read_to_string(identity_path)
                        .unwrap()
                        .contains(&row.uuid),
                    "the failed append left its identity registered"
                );
                assert!(
                    !writer
                        .read_session_message_identity_snapshot(&fixture.transcript_path)
                        .await
                        .unwrap()
                        .by_uuid
                        .contains_key(&row.uuid),
                    "identity snapshot must exclude a missing physical row"
                );
                fixture.orch.session.lock().await.history.push(derived);
                let summary =
                    ConversationMessage::user(MessageId::new(), "summary of old context".into());
                let summary_id = summary.id();
                let compacted = fixture
                    .orch
                    .apply_post_compact(
                        compaction::IterationCompactionResult {
                            messages: vec![summary],
                            raw_summary_text: "summary of old context".into(),
                            layers_applied: vec![],
                            total_tokens_freed: 1,
                            cache_hit: false,
                            consecutive_failures: 0,
                            was_compacted: true,
                            rapid_refill_breaker_tripped: false,
                            consecutive_rapid_refills: 0,
                            messages_to_preserve: vec![],
                            media_analysis_to_preserve: vec![],
                            compaction_usage: None,
                            compaction_model: None,
                        },
                        compaction::CompactTrigger::Auto,
                        100,
                        4,
                        100,
                        std::time::Instant::now(),
                        None,
                    )
                    .await;
                assert!(compacted.is_some());
                let history = fixture.orch.session.lock().await.history.clone();
                let origin = history
                    .iter()
                    .position(|message| message.id() == origin_id)
                    .expect("unfinished origin must survive full replacement compaction");
                let last = history
                    .iter()
                    .position(|message| message.id() == later_id)
                    .unwrap();
                assert!(
                    history
                        .iter()
                        .position(|message| message.id() == summary_id)
                        .unwrap()
                        < origin
                );
                assert!(origin < last);
                if fresh {
                    let session_id = fixture.orch.session.lock().await.session_id;
                    let loaded = session::jsonl::load_session(
                        fixture._root.path(),
                        &fixture._root.path().to_string_lossy(),
                        session_id.as_uuid(),
                        Arc::new(platform_posix::fs::PosixFileSystem::new(
                            fixture._root.path().to_path_buf(),
                        )),
                    )
                    .await
                    .unwrap();
                    let cold = crate::resume::state_from_messages(session_id.as_uuid(), &loaded);
                    assert!(
                        cold.history.iter().any(|message| message.id() == origin_id),
                        "persisted compact tail must restore the exact origin"
                    );
                    assert!(cold.history.iter().any(|message| message.id() == later_id));
                    assert!(
                        cold.history
                            .iter()
                            .any(|message| message.id() == derived_id),
                        "a derived in-memory tail endpoint must become durable before compaction"
                    );
                    assert!(cold.history.iter().any(|message| message.id()==exact_id && matches!(message,
                    ConversationMessage::User {content,..} if content.iter().any(|block| matches!(block,
                        ContentBlock::TextJsUtf16 {utf16_code_units,..} if utf16_code_units==&vec![0xd800])))),
                    "an existing exact UTF16 row must survive without timestamp or publication conflicts: {:?}", cold.history.iter().find(|message| message.id()==exact_id));
                    let mut session = fixture.orch.session.lock().await;
                    session.history = cold.history;
                    session.model_context_excluded_messages = cold.model_context_excluded_messages;
                    drop(session);
                    fixture.forget_runtime().await;
                }
                *fixture.api.submission_probe.lock().unwrap() =
                    Some(Arc::new(llm_runtime::computer::ComputerReceiptSubmission {
                        journal: fixture.journal.clone(),
                        receipts: vec![saved.clone()],
                    }));
                let result = crate::turn_loop::execute_one_turn(&fixture.orch, None)
                    .await
                    .unwrap();
                assert!(matches!(
                    result,
                    crate::turn_loop::TurnStepOutcome::Ended { .. }
                ));
                assert_eq!(fixture.api.provider_inputs.lock().unwrap().len(), 1);
                assert_eq!(
                    fixture
                        .journal
                        .receipt(&saved.receipt_id)
                        .await
                        .unwrap()
                        .unwrap()
                        .stage,
                    NativeReceiptStage::ResponseReceived
                );
                assert_eq!(
                    fixture.computer.calls.lock().unwrap().len(),
                    1,
                    "compaction recovery must never repeat desktop input"
                );
                assert!(
                    super::preserve_pending_compaction_tail(&fixture.orch, &[])
                        .await
                        .unwrap()
                        .is_empty(),
                    "completed receipts must no longer pin history"
                );
            }
        }
    }
}
