use super::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tool_api::context::HostToolCancellation;

struct HookFixture {
    _directory: tempfile::TempDir,
    marker: PathBuf,
    called: Arc<AtomicBool>,
    orch: ConversationOrchestrator,
    executor: Arc<hooks::HookExecutorImpl>,
    session_id: lingxi_core::types::SessionId,
    completions: tokio::sync::mpsc::Receiver<hooks::async_registry::HookCompletionEnvelope>,
}

struct CountingBuiltin(Arc<AtomicUsize>);

struct BlockingToolCallOutput {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    published: Arc<AtomicUsize>,
}

#[async_trait]
impl lingxi_core::host::OutputStream for BlockingToolCallOutput {
    async fn emit_text(&self, _: &lingxi_core::types::utf16_json::Utf16JsonProjection) {}
    async fn emit_tool_call(
        &self,
        _: &ToolUseId,
        _: &str,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) {
        self.entered.notify_one();
        self.release.notified().await;
        self.published.fetch_add(1, Ordering::SeqCst);
    }
    async fn emit_tool_result(
        &self,
        _: &ToolUseId,
        _: &str,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
    ) {
    }
    async fn emit_end_turn(&self, _: &str, _: &lingxi_core::host::CostSnapshot) {}
}

struct BarrierBuiltin {
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    response: Option<hooks::HookResponse>,
}

#[async_trait]
impl hooks::BuiltinHookHandler for BarrierBuiltin {
    fn id(&self) -> &str {
        "host-hook-barrier"
    }
    fn supports_owned_host_request(&self) -> bool {
        true
    }
    async fn handle(&self, _: &hooks::HookEvent, _: &hooks::HookContext) -> hooks::HookResult {
        self.entered.notify_one();
        self.release.notified().await;
        hooks::HookResult {
            outcome: hooks::HookOutcome::Success,
            stdout: "".into(),
            stderr: "".into(),
            exit_code: None,
            response: self.response.clone(),
        }
    }
}

fn builtin_definition(id: &str, priority: i32, once: bool) -> HookDefinition {
    HookDefinition {
        id: HookId::new(),
        name: id.into(),
        events: vec![HookEventType::PreToolUse],
        if_condition: None,
        executor: DefHookExecutor::Builtin {
            handler_id: id.into(),
        },
        source: HookSource::Session,
        blocking: true,
        timeout: None,
        priority,
        once,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
        on_failure: None,
    }
}

async fn barrier_orchestrator(
    cwd: PathBuf,
    response: Option<hooks::HookResponse>,
    mod_registry: Option<Arc<tokio::sync::RwLock<hooks::HookRegistry>>>,
) -> (
    Arc<ConversationOrchestrator>,
    Arc<hooks::HookExecutorImpl>,
    Arc<AtomicBool>,
    Arc<AtomicUsize>,
    Arc<tokio::sync::Notify>,
    Arc<tokio::sync::Notify>,
    lingxi_core::types::SessionId,
) {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let called = Arc::new(AtomicBool::new(false));
    let mut executor =
        Arc::try_unwrap(noop_hook_executor()).unwrap_or_else(|_| panic!("unique test executor"));
    executor.register_builtin(Arc::new(BarrierBuiltin {
        entered: entered.clone(),
        release: release.clone(),
        response,
    }));
    executor.register_builtin(Arc::new(CountingBuiltin(calls.clone())));
    let executor = Arc::new(executor);
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(BashDispatchProbeTool {
        name: "Bash",
        called: called.clone(),
        permission_checks: None,
        observed_host_policy: None,
    }));
    let mut orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tools),
        executor.clone(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd,
    );
    if let Some(registry) = mod_registry {
        orch = orch.with_hook_registry(registry);
    }
    let orch = ConversationOrchestrator::into_shared(orch);
    let session_id = orch.session.lock().await.session_id;
    executor
        .upsert_session_named_hook(
            session_id,
            "first".into(),
            builtin_definition("host-hook-barrier", 10, true),
        )
        .await;
    executor
        .upsert_session_named_hook(
            session_id,
            "second".into(),
            builtin_definition("host-hook-safe-builtin", 0, false),
        )
        .await;
    (orch, executor, called, calls, entered, release, session_id)
}

struct HookPermissionGate {
    classifier_deny: bool,
}

#[async_trait]
impl permission::PermissionGate for HookPermissionGate {
    async fn check(
        &self,
        _: &str,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> crate::test_support::PermissionDecision {
        crate::test_support::PermissionDecision::Deny {
            reason: "fixture denied".into(),
        }
    }

    async fn resolve_detailed(
        &self,
        _: &str,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> crate::test_support::PermissionResolution {
        if self.classifier_deny {
            crate::test_support::PermissionResolution::Deny {
                reason: "fixture denied".into(),
                source: crate::test_support::PermissionDecisionSource::Classifier,
                rule_source: None,
                decision_reason_type: Some("classifier".into()),
                decision_reason: None,
                behavior_ask: false,
                content_blocks: Vec::new(),
            }
        } else {
            crate::test_support::PermissionResolution::Ask
        }
    }
}

#[async_trait]
impl hooks::BuiltinHookHandler for CountingBuiltin {
    fn supports_owned_host_request(&self) -> bool {
        true
    }
    fn id(&self) -> &str {
        "host-hook-safe-builtin"
    }

    async fn handle(&self, _: &hooks::HookEvent, _: &hooks::HookContext) -> hooks::HookResult {
        self.0.fetch_add(1, Ordering::SeqCst);
        hooks::HookResult {
            outcome: hooks::HookOutcome::Success,
            stdout: String::new().into(),
            stderr: String::new().into(),
            exit_code: None,
            response: None,
        }
    }
}

async fn fixture(
    event: HookEventType,
    blocking: bool,
    matcher: Option<&str>,
    builtin_count: Option<Arc<AtomicUsize>>,
) -> HookFixture {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("command-hook-started");
    let called = Arc::new(AtomicBool::new(false));
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(BashDispatchProbeTool {
        name: "Bash",
        called: called.clone(),
        permission_checks: None,
        observed_host_policy: None,
    }));
    let (completion_tx, completions) = tokio::sync::mpsc::channel(8);
    let async_hooks = Arc::new(hooks::AsyncHookRegistry::new(
        Arc::new(platform_posix::PosixRuntime::new()),
        completion_tx,
    ));
    let mut executor = Arc::try_unwrap(noop_hook_executor())
        .unwrap_or_else(|_| panic!("test hook executor must be uniquely owned"))
        .with_process_runner(
            Arc::new(platform_posix::PosixProcess::new()),
            Arc::new(platform_posix::PosixSandbox::new()),
        )
        .with_async_registry(async_hooks);
    let hook_executor = if let Some(count) = builtin_count {
        executor.register_builtin(Arc::new(CountingBuiltin(count)));
        DefHookExecutor::Builtin {
            handler_id: "host-hook-safe-builtin".into(),
        }
    } else {
        DefHookExecutor::Command {
            command: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "printf started > \"$TEST_HOOK_MARKER\"; printf '{}\\n'".into(),
            ],
            env: HashMap::from([(
                "TEST_HOOK_MARKER".into(),
                marker.to_string_lossy().into_owned(),
            )]),
            cwd: None,
            shell: None,
        }
    };
    let executor = Arc::new(executor);
    let gate: Arc<dyn permission::PermissionGate> = match &event {
        HookEventType::PermissionRequest => Arc::new(HookPermissionGate {
            classifier_deny: false,
        }),
        HookEventType::PermissionDenied => Arc::new(HookPermissionGate {
            classifier_deny: true,
        }),
        _ => Arc::new(NoOpPermissionGate),
    };
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tools),
        executor.clone(),
        gate,
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        directory.path().to_path_buf(),
    );
    let session_id = orch.session.lock().await.session_id;
    executor
        .upsert_session_named_hook(
            session_id,
            "host-hook-fixture".into(),
            HookDefinition {
                id: HookId::new(),
                name: "host-hook-fixture".into(),
                events: vec![event],
                if_condition: matcher.map(|pattern| hooks::HookCondition {
                    pattern: pattern.into(),
                    match_tool_name: true,
                    match_input: false,
                    if_pattern: None,
                }),
                executor: hook_executor,
                source: HookSource::Session,
                blocking,
                timeout: None,
                priority: 0,
                once: false,
                status_message: None,
                async_rewake: false,
                async_timeout: None,
                rewake_message: None,
                on_failure: None,
            },
        )
        .await;
    HookFixture {
        _directory: directory,
        marker,
        called,
        orch,
        executor,
        session_id,
        completions,
    }
}

impl HookFixture {
    async fn call(
        &self,
        command: &str,
        strict: bool,
    ) -> Result<Option<lingxi_core::types::ContentBlock>, crate::error::OrchestratorError> {
        let cancellation = if strict {
            HostToolCancellation::TerminateOwnedProcess(Arc::new(Default::default()))
        } else {
            HostToolCancellation::Ordinary
        };
        self.orch
            .call_tool_from_host_with_cancellation(
                ToolUseId::new(),
                "Bash".into(),
                source_fixture!({"command":command}),
                None,
                cancellation,
            )
            .await
    }
}

#[tokio::test]
async fn matching_real_command_hooks_are_rejected_before_start_for_owned_requests() {
    let events = [
        (HookEventType::PreToolUse, "ok", true),
        (HookEventType::PermissionRequest, "ok", false),
        (HookEventType::PermissionDenied, "ok", false),
        (HookEventType::PostToolUse, "ok", true),
        (HookEventType::PostToolUseFailure, "hook-test-failure", true),
        (HookEventType::PostToolBatch, "ok", true),
    ];
    for (event, command, ordinary_tool_runs) in events {
        for blocking in [true, false] {
            let mut fixture = fixture(event.clone(), blocking, None, None).await;
            let error = fixture.call(command, true).await.unwrap_err();
            assert!(
                error.to_string().contains("command hook"),
                "{event:?}: {error}"
            );
            assert!(!error.to_string().contains("tool has already run"));
            assert!(
                !fixture.marker.exists(),
                "{event:?} hook started in strict scope"
            );
            assert!(!fixture.called.load(Ordering::SeqCst));

            fixture.called.store(false, Ordering::SeqCst);
            let ordinary = fixture.call(command, false).await.unwrap().unwrap();
            assert_eq!(fixture.called.load(Ordering::SeqCst), ordinary_tool_runs);
            assert!(matches!(
                ordinary,
                lingxi_core::types::ContentBlock::ToolResult { .. }
            ));
            if !blocking {
                let completion = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    fixture.completions.recv(),
                )
                .await
                .expect("ordinary async hook completed")
                .expect("async hook completion channel is open");
                assert_eq!(completion.result.outcome, hooks::HookOutcome::Success);
            }
            assert!(
                fixture.marker.exists(),
                "ordinary {event:?} command did not run"
            );
        }
    }
}

#[tokio::test]
async fn owned_hook_cancellation_waits_for_admitted_hook_and_stops_the_next() {
    let directory = tempfile::tempdir().unwrap();
    let (orch, executor, called, calls, entered, release, session_id) =
        barrier_orchestrator(directory.path().to_path_buf(), None, None).await;
    let request = Arc::new(tool_api::context::HostToolRequest::default());
    let task = tokio::spawn({
        let orch = orch.clone();
        let request = request.clone();
        async move {
            orch.call_tool_from_host_with_cancellation(
                ToolUseId::new(),
                "Bash".into(),
                source_fixture!({"command":"ok"}),
                None,
                HostToolCancellation::TerminateOwnedProcess(request),
            )
            .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    assert!(request.cancel());
    let mut cleanup = tokio::spawn({
        let request = request.clone();
        async move { request.cancel_and_reap().await }
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut cleanup)
            .await
            .is_err(),
        "cleanup must wait for admitted hook"
    );
    release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());
    tokio::time::timeout(std::time::Duration::from_secs(5), cleanup)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0, "later hook must not start");
    assert!(!called.load(Ordering::SeqCst));
    assert!(
        executor
            .get_session_named_hook(session_id, "first")
            .await
            .is_some(),
        "cancelled success must not consume once"
    );
}

#[tokio::test]
async fn owned_direct_dispatch_publishes_its_deferred_result_frame() {
    let directory = tempfile::tempdir().unwrap();
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(BashDispatchProbeTool {
        name: "Bash",
        called: Arc::new(AtomicBool::new(false)),
        permission_checks: None,
        observed_host_policy: None,
    }));
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tools),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        directory.path().to_path_buf(),
    );
    let id = ToolUseId::new();
    assert!(orch
        .call_tool_from_host_with_cancellation(
            id.clone(),
            "Bash".into(),
            source_fixture!({"command":"ok"}),
            None,
            HostToolCancellation::TerminateOwnedProcess(Arc::new(Default::default())),
        )
        .await
        .unwrap()
        .is_some());
    assert!(
        output.snapshot().await.iter().any(|event| matches!(event,
            lingxi_core::host::OutputEvent::ToolResult { id: actual, .. } if actual == &id
        )),
        "direct host result frame must not be dropped"
    );
}

#[tokio::test]
async fn owned_tool_call_output_drops_when_request_is_cancelled() {
    let directory = tempfile::tempdir().unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let published = Arc::new(AtomicUsize::new(0));
    let called = Arc::new(AtomicBool::new(false));
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(BashDispatchProbeTool {
        name: "Bash",
        called: called.clone(),
        permission_checks: None,
        observed_host_policy: None,
    }));
    let output = Arc::new(BlockingToolCallOutput {
        entered: entered.clone(),
        release: release.clone(),
        published: published.clone(),
    });
    let orch = Arc::new(ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tools),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output,
        Arc::new(StaticMemoryProvider::empty()),
        directory.path().to_path_buf(),
    ));
    let request = Arc::new(tool_api::context::HostToolRequest::default());
    let task = tokio::spawn({
        let request = request.clone();
        async move {
            orch.call_tool_from_host_with_cancellation(
                ToolUseId::new(),
                "Bash".into(),
                source_fixture!({"command":"ok"}),
                None,
                HostToolCancellation::TerminateOwnedProcess(request),
            )
            .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    assert!(request.cancel());
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());
    release.notify_one();
    assert_eq!(published.load(Ordering::SeqCst), 0);
    assert!(!called.load(Ordering::SeqCst));
}

#[tokio::test]
async fn owned_mod_synthetic_tool_calls_drop_when_request_is_cancelled() {
    for answer in [
        "{result:{ok:true}}",
        "{deny:'fixture denial'}",
        "{result:{force_test_output_error:true}}",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let module = directory.path().join("short-circuit.js");
        std::fs::write(&module, format!("export function register(on) {{ on('tool.call', {{tool:'Bash'}}, () => ({answer})); }}")).unwrap();
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load(
            "short-circuit",
            directory.path(),
            &module,
            source_fixture!({}),
        )
        .await
        .unwrap();
        let mut hooks = hooks::HookRegistry::new();
        hooks.set_mod_host(host);
        let mut tools = ToolRegistry::new();
        let called = Arc::new(AtomicBool::new(false));
        tools.register_builtin(Arc::new(BashDispatchProbeTool {
            name: "Bash",
            called: called.clone(),
            permission_checks: None,
            observed_host_policy: None,
        }));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let published = Arc::new(AtomicUsize::new(0));
        let output = Arc::new(BlockingToolCallOutput {
            entered: entered.clone(),
            release: release.clone(),
            published: published.clone(),
        });
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new(
                OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![])),
                Arc::new(tools),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                output,
                Arc::new(StaticMemoryProvider::empty()),
                directory.path().to_path_buf(),
            )
            .with_hook_registry(Arc::new(tokio::sync::RwLock::new(hooks))),
        );
        let request = Arc::new(tool_api::context::HostToolRequest::default());
        let task = tokio::spawn({
            let request = request.clone();
            async move {
                orch.call_tool_from_host_with_cancellation(
                    ToolUseId::new(),
                    "Bash".into(),
                    source_fixture!({"command":"ok"}),
                    None,
                    HostToolCancellation::TerminateOwnedProcess(request),
                )
                .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        assert!(request.cancel());
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err(), "{answer}");
        release.notify_one();
        assert_eq!(published.load(Ordering::SeqCst), 0, "{answer}");
        assert!(!called.load(Ordering::SeqCst), "{answer}");
    }
}

#[tokio::test]
async fn owned_hook_allow_keeps_admitted_mod_check_after_live_host_is_cleared() {
    let directory = tempfile::tempdir().unwrap();
    let module = directory.path().join("mod-deny.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
      on('tool.check', {tool:'Bash'}, async ($) => {
        await $.fs.write('checked', 'yes'); return {decision:'deny', reason:'fixture denial'};
      });
    }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("deny", directory.path(), &module, source_fixture!({}))
        .await
        .unwrap();
    let mut hooks = hooks::HookRegistry::new();
    hooks.set_mod_host(host);
    let registry = Arc::new(tokio::sync::RwLock::new(hooks));
    let response = hooks::HookResponse {
        decision: Some(hooks::HookDecision::Allow),
        ..Default::default()
    };
    let (orch, _, called, _, entered, release, _) = barrier_orchestrator(
        directory.path().to_path_buf(),
        Some(response),
        Some(registry.clone()),
    )
    .await;
    let task = tokio::spawn(async move {
        orch.call_tool_from_host_with_cancellation(
            ToolUseId::new(),
            "Bash".into(),
            source_fixture!({"command":"ok"}),
            None,
            HostToolCancellation::TerminateOwnedProcess(Arc::new(Default::default())),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    registry.write().await.clear_mod_host();
    release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(result.is_some());
    assert!(
        directory.path().join("checked").exists(),
        "admission-time Mod check must run"
    );
    assert!(!called.load(Ordering::SeqCst), "denied shell must not run");
}

#[tokio::test]
async fn owned_requests_allow_nonmatching_commands_and_safe_builtin_hooks() {
    let unmatched = fixture(HookEventType::PreToolUse, true, Some("Read"), None).await;
    assert!(unmatched.call("ok", true).await.unwrap().is_some());
    assert!(unmatched.called.load(Ordering::SeqCst));
    assert!(!unmatched.marker.exists());

    let other_shell = fixture(HookEventType::PreToolUse, true, Some("PowerShell"), None).await;
    assert!(other_shell.call("ok", true).await.unwrap().is_some());
    assert!(other_shell.called.load(Ordering::SeqCst));
    assert!(!other_shell.marker.exists());

    let builtin_calls = Arc::new(AtomicUsize::new(0));
    let safe = fixture(
        HookEventType::PreToolUse,
        true,
        None,
        Some(builtin_calls.clone()),
    )
    .await;
    assert!(safe.call("ok", true).await.unwrap().is_some());
    assert!(safe.called.load(Ordering::SeqCst));
    assert_eq!(builtin_calls.load(Ordering::SeqCst), 1);

    let background_calls = Arc::new(AtomicUsize::new(0));
    let mut background = fixture(
        HookEventType::PreToolUse,
        false,
        None,
        Some(background_calls.clone()),
    )
    .await;
    let error = background.call("ok", true).await.unwrap_err();
    assert!(error.to_string().contains("background hook"));
    assert_eq!(background_calls.load(Ordering::SeqCst), 0);
    assert!(!background.called.load(Ordering::SeqCst));
    assert!(background.call("ok", false).await.unwrap().is_some());
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        background.completions.recv(),
    )
    .await
    .expect("ordinary background hook completed")
    .expect("background completion channel is open");
    assert_eq!(background_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn owned_request_preflights_other_shell_only_when_mod_can_invoke_it() {
    let mut fixture = fixture(HookEventType::PostToolUse, true, Some("PowerShell"), None).await;
    let module = fixture._directory.path().join("tool-call.js");
    std::fs::write(
        &module,
        "export function register(on) { on('tool.call', {tool:'Bash'}, ($, e, next) => next(e)); }",
    )
    .unwrap();
    let mod_host = hooks::mods::ModHost::start(None).await.unwrap();
    mod_host
        .load(
            "nested-shell",
            fixture._directory.path(),
            &module,
            source_fixture!({}),
        )
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(mod_host);
    fixture.orch = fixture
        .orch
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    let error = fixture.call("ok", true).await.unwrap_err();
    assert!(error.to_string().contains("command hook"));
    assert!(!fixture.called.load(Ordering::SeqCst));
    assert!(!fixture.marker.exists());
}

#[tokio::test]
async fn owned_mod_forbidden_api_does_not_run_its_middleware_or_fall_back_to_shell() {
    for (event, operation, value) in [
        (
            "process.run",
            "await $.process.run(['/bin/sh', '-c', 'exit 0'])",
            "{exitCode:0,stdout:'',stderr:''}",
        ),
        ("session.repo", "await $.session.repo()", "null"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let module = directory.path().join("api-guard.js");
        std::fs::write(&module, format!(r#"export function register(on) {{
          on('{event}', async ($) => {{ await $.fs.write('middleware-marker', 'ran'); return {{value:{value}}}; }});
          on('tool.call', {{tool:'Bash'}}, async ($, e, next) => {{
            await $.fs.write('before-marker', 'ran'); {operation}; return next(e);
          }});
        }}"#)).unwrap();
        let host = hooks::mods::ModHost::start(None).await.unwrap();
        host.load("api-guard", directory.path(), &module, source_fixture!({}))
            .await
            .unwrap();
        let mut hooks = hooks::HookRegistry::new();
        hooks.set_mod_host(host);
        let called = Arc::new(AtomicBool::new(false));
        let orch = ConversationOrchestrator::into_shared(
            orch_with_tools(
                directory.path().to_path_buf(),
                vec![Arc::new(BashDispatchProbeTool {
                    name: "Bash",
                    called: called.clone(),
                    permission_checks: None,
                    observed_host_policy: None,
                })],
            )
            .with_hook_registry(Arc::new(tokio::sync::RwLock::new(hooks))),
        );
        let strict = orch
            .call_tool_from_host_with_cancellation(
                ToolUseId::new(),
                "Bash".into(),
                source_fixture!({"command":"strict"}),
                None,
                HostToolCancellation::TerminateOwnedProcess(Arc::new(Default::default())),
            )
            .await
            .unwrap_err();
        assert!(strict.to_string().contains(event), "{event}: {strict}");
        assert!(
            directory.path().join("before-marker").exists(),
            "pre-existing allowed work is visible"
        );
        assert!(!directory.path().join("middleware-marker").exists());
        assert!(!called.load(Ordering::SeqCst));
        assert!(orch
            .call_tool_from_host(
                ToolUseId::new(),
                "Bash".into(),
                source_fixture!({"command":"ordinary"}),
                None
            )
            .await
            .unwrap()
            .is_some());
        assert!(directory.path().join("middleware-marker").exists());
        assert!(called.load(Ordering::SeqCst));
    }
}

#[tokio::test]
async fn owned_mod_denial_is_recorded_before_a_concurrent_next() {
    let directory = tempfile::tempdir().unwrap();
    let module = directory.path().join("concurrent-denial.js");
    std::fs::write(
        &module,
        r#"export function register(on) {
      on('tool.call', {tool:'Bash'}, async ($, e, next) => {
        const bad = $.process.run(['/bin/sh', '-c', 'exit 0']).catch(() => null);
        const result = await next(e); await bad; return result;
      });
    }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("concurrent", directory.path(), &module, source_fixture!({}))
        .await
        .unwrap();
    let mut hooks = hooks::HookRegistry::new();
    hooks.set_mod_host(host);
    let called = Arc::new(AtomicBool::new(false));
    let orch = ConversationOrchestrator::into_shared(
        orch_with_tools(
            directory.path().to_path_buf(),
            vec![Arc::new(BashDispatchProbeTool {
                name: "Bash",
                called: called.clone(),
                permission_checks: None,
                observed_host_policy: None,
            })],
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(hooks))),
    );
    let error = orch
        .call_tool_from_host_with_cancellation(
            ToolUseId::new(),
            "Bash".into(),
            source_fixture!({"command":"ok"}),
            None,
            HostToolCancellation::TerminateOwnedProcess(Arc::new(Default::default())),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("process.run"));
    assert!(!called.load(Ordering::SeqCst));
}

#[tokio::test]
async fn nested_shell_must_be_in_the_admission_snapshot() {
    let directory = tempfile::tempdir().unwrap();
    let tools: Vec<Arc<dyn Tool>> = ["Bash", "PowerShell"]
        .into_iter()
        .map(|name| {
            Arc::new(BashDispatchProbeTool {
                name,
                called: Arc::new(AtomicBool::new(false)),
                permission_checks: None,
                observed_host_policy: None,
            }) as Arc<dyn Tool>
        })
        .collect();
    let orch = ConversationOrchestrator::into_shared(orch_with_tools(
        directory.path().to_path_buf(),
        tools,
    ));
    let session_id = orch.session.lock().await.session_id;
    let snapshot = orch
        .hooks
        .snapshot_for_owned_host_request(
            &["Bash".into()],
            &hooks::HookContext {
                session_id,
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let request = Arc::new(tool_api::context::HostToolRequest::default());
    let session = tool_api::context::with_host_tool_cancellation(
        HostToolCancellation::TerminateOwnedProcess(request),
        hooks::executor::with_owned_host_hook_snapshot(snapshot, async {
            crate::turn_loop::tool_dispatch::scoped_mod_session_for_test(&orch).unwrap()
        }),
    )
    .await;
    let error = match session
        .prepare_tool_call(
            "test",
            "PowerShell".into(),
            None,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
    {
        Ok(_) => panic!("unpreflighted nested shell must be rejected"),
        Err(error) => error,
    };
    assert!(error
        .to_string()
        .contains("not included in host hook preflight"));
    assert!(session
        .prepare_tool_call(
            "test",
            "Bash".into(),
            None,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .is_ok());
}

#[tokio::test]
async fn snapshot_does_not_repeat_a_consumed_once_hook_on_post() {
    let calls = Arc::new(AtomicUsize::new(0));
    let fixture = fixture(HookEventType::PreToolUse, true, None, Some(calls.clone())).await;
    fixture
        .executor
        .upsert_session_named_hook(
            fixture.session_id,
            "host-hook-fixture".into(),
            HookDefinition {
                id: HookId::new(),
                name: "once-pre-post".into(),
                events: vec![HookEventType::PreToolUse, HookEventType::PostToolUse],
                if_condition: None,
                executor: DefHookExecutor::Builtin {
                    handler_id: "host-hook-safe-builtin".into(),
                },
                source: HookSource::Session,
                blocking: true,
                timeout: None,
                priority: 0,
                once: true,
                status_message: None,
                async_rewake: false,
                async_timeout: None,
                rewake_message: None,
                on_failure: None,
            },
        )
        .await;
    assert!(fixture.call("ok", true).await.unwrap().is_some());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
