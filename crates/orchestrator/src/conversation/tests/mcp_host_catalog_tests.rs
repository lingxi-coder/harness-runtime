use super::*;
use crate::test_support::{
    noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use async_trait::async_trait;
use lingxi_core::host::task_registry::{
    TaskCreateInput, TaskListFilter, TaskOutputChunk, TaskRecord, TaskRegistryError,
    TaskRegistryHandle, TaskUpdatePatch,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock, ToolUseId};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    generated_builtin_result, DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext, ValidationError,
};
use tool_api::{DeferralState, ToolSearchMode};

#[derive(Default)]
struct CountingApi(AtomicUsize);

#[async_trait]
impl OrchestratorApiClient for CountingApi {
    async fn messages_create(
        &self,
        _request: crate::OrchestratorApiRequest,
    ) -> Result<HistoryResponse, LlmError> {
        Err(LlmError::ModelUnavailable)
    }

    async fn count_tokens_exact(
        &self,
        _model: &str,
        _profile: Option<&str>,
        _system: Option<&str>,
        _messages: Vec<ConversationMessage>,
        _tools: Vec<lingxi_core::host::SourceToolWireDefinition>,
    ) -> Result<Option<u64>, LlmError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Some(100_000))
    }
}

#[derive(Default)]
struct FixtureTaskRegistry {
    reads: AtomicUsize,
    outputs: AtomicUsize,
    stops: AtomicUsize,
    calls: Mutex<Vec<(&'static str, String)>>,
}

impl FixtureTaskRegistry {
    fn record(&self, operation: &'static str, id: &str) {
        self.calls.lock().unwrap().push((operation, id.to_owned()));
    }

    fn assert_calls(&self, expected: &[(&str, &str)]) {
        let calls = self.calls.lock().unwrap();
        let actual: Vec<_> = calls
            .iter()
            .map(|(operation, id)| (*operation, id.as_str()))
            .collect();
        assert_eq!(actual.as_slice(), expected);
    }
}

const OUTPUT_CALLS: &[(&str, &str)] = &[
    ("get", "b12345678"),
    ("output", "b12345678"),
    ("get", "b12345678"),
];
const OUTPUT_AND_STOP_CALLS: &[(&str, &str)] = &[
    ("get", "b12345678"),
    ("output", "b12345678"),
    ("get", "b12345678"),
    ("get", "b12345678"),
    ("kill", "b12345678"),
];

fn running_task() -> TaskRecord {
    TaskRecord {
        task_id: "b12345678".into(),
        task_type: "local_bash".into(),
        status: "running".into(),
        description: "fixture shell".into(),
        ..Default::default()
    }
}

#[async_trait]
impl TaskRegistryHandle for FixtureTaskRegistry {
    async fn create(&self, _: TaskCreateInput) -> Result<TaskRecord, TaskRegistryError> {
        unreachable!("the fixture does not create tasks")
    }
    async fn get(&self, id: &str) -> Result<Option<TaskRecord>, TaskRegistryError> {
        self.record("get", id);
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok((id == "b12345678").then(running_task))
    }
    async fn list(&self, _: TaskListFilter) -> Result<Vec<TaskRecord>, TaskRegistryError> {
        Ok(Vec::new())
    }
    async fn update(&self, _: &str, _: TaskUpdatePatch) -> Result<TaskRecord, TaskRegistryError> {
        unreachable!("the fixture does not update tasks")
    }
    async fn set_status(&self, _: &str, _: &str) -> Result<TaskRecord, TaskRegistryError> {
        unreachable!("the fixture does not change task status directly")
    }
    async fn kill(&self, id: &str) -> Result<TaskRecord, TaskRegistryError> {
        assert_eq!(id, "b12345678");
        self.record("kill", id);
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(running_task())
    }
    async fn output(&self, id: &str, _: Option<u64>) -> Result<TaskOutputChunk, TaskRegistryError> {
        assert_eq!(id, "b12345678");
        self.record("output", id);
        self.outputs.fetch_add(1, Ordering::SeqCst);
        Ok(TaskOutputChunk {
            task_id: id.to_owned(),
            content: "fixture output".into(),
            status: Some("running".into()),
            ..Default::default()
        })
    }
}

struct MarkerTool {
    name: &'static str,
    enabled: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    validation_gate: Option<Arc<tokio::sync::Barrier>>,
    gate_entered: AtomicBool,
}

#[async_trait]
impl Tool for MarkerTool {
    fn name(&self) -> &str {
        self.name
    }

    fn input_schema(&self) -> &Utf16JsonProjection {
        static SCHEMA: std::sync::OnceLock<Utf16JsonProjection> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| generated_builtin_result(&json!({"type":"object","properties":{}})))
    }

    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        tool_api::tool_trait::ToolInputRepairMetadata::Known {
            schema: tool_api::tool_trait::ToolInputRepairSchema::Advertised,
            schema_provenance: tool_api::tool_trait::ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }

    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    fn max_result_size_chars(&self) -> usize {
        1024
    }

    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        true
    }

    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        true
    }

    async fn validate_input(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        if !self.gate_entered.swap(true, Ordering::SeqCst) {
            if let Some(gate) = &self.validation_gate {
                gate.wait().await;
                gate.wait().await;
            }
        }
        Ok(())
    }

    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "host catalog fixture".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        self.name.to_owned()
    }

    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        self.name.into()
    }

    async fn call(
        &self,
        _: Utf16JsonProjection,
        _: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ToolCallResult {
            data: generated_builtin_result(&json!({"content":"ok"})),
            model_content: None,
            new_messages: Vec::new(),
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
            subagent_hooks_fired: false,
        })
    }
}

struct Fixture {
    orchestrator: ConversationOrchestrator,
    api: Arc<CountingApi>,
    tasks: Arc<FixtureTaskRegistry>,
    wait_enabled: Arc<AtomicBool>,
    wait_calls: Arc<AtomicUsize>,
}

fn fixture(
    allowlist: Option<&[&str]>,
    permission_gate: Arc<dyn permission::PermissionGate>,
) -> Fixture {
    let api = Arc::new(CountingApi::default());
    let tasks = Arc::new(FixtureTaskRegistry::default());
    let wait_enabled = Arc::new(AtomicBool::new(true));
    let wait_calls = Arc::new(AtomicUsize::new(0));
    let mut ctx = tool_api::test_support::ctx_for_file_tools(
        tool_api::test_support::make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![std::env::temp_dir()],
    );
    ctx.task_registry = Some(tasks.clone());
    let mut registry = ToolRegistry::new();
    registry.set_deferral(Arc::new(DeferralState::new(
        ToolSearchMode::Auto { percentage: 1 },
        false,
    )));
    registry.register_builtin(Arc::new(tool_task::TaskOutputTool::new(ctx.clone())));
    registry.register_builtin(Arc::new(tool_task::TaskStopTool::new(ctx)));
    registry.register_builtin(Arc::new(MarkerTool {
        name: "ToolSearch",
        enabled: Arc::new(AtomicBool::new(true)),
        calls: Arc::new(AtomicUsize::new(0)),
        validation_gate: None,
        gate_entered: AtomicBool::new(false),
    }));
    registry.register_builtin(Arc::new(MarkerTool {
        name: "WaitForMcpServers",
        enabled: wait_enabled.clone(),
        calls: wait_calls.clone(),
        validation_gate: None,
        gate_entered: AtomicBool::new(false),
    }));
    if let Some(names) = allowlist {
        registry.set_session_tool_allowlist(
            &names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>(),
        );
    }
    let mut config = OrchestratorConfig::default();
    config.model = "claude-sonnet-4-5".into();
    let orchestrator = ConversationOrchestrator::new(
        config,
        api.clone(),
        Arc::new(registry),
        noop_hook_executor(),
        permission_gate,
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(Vec::new())),
        PathBuf::from("/work/repo"),
    );
    Fixture {
        orchestrator,
        api,
        tasks,
        wait_enabled,
        wait_calls,
    }
}

fn named<'a>(catalog: &'a [Utf16JsonProjection], name: &str) -> Option<&'a Utf16JsonProjection> {
    catalog
        .iter()
        .find(|item| item.string_at("/name") == Some(name))
}

async fn call(orch: &ConversationOrchestrator, name: &str) -> Option<ContentBlock> {
    let input = if name == "TaskOutput" {
        generated_builtin_result(&json!({"task_id":"b12345678", "block":false}))
    } else if name.starts_with("Task") {
        generated_builtin_result(&json!({"task_id":"b12345678"}))
    } else {
        generated_builtin_result(&json!({}))
    };
    orch.call_tool_from_host(ToolUseId::new(), name.to_owned(), input, None)
        .await
        .expect("host dispatch")
}

#[tokio::test]
async fn host_catalog_exposes_real_deferred_task_tools_without_provider_counting() {
    let fixture = fixture(None, Arc::new(NoOpPermissionGate));
    let catalog = fixture.orchestrator.mcp_tool_definitions().await.unwrap();
    assert_eq!(fixture.api.0.load(Ordering::SeqCst), 0);
    for name in ["TaskOutput", "TaskStop", "WaitForMcpServers"] {
        assert!(named(&catalog, name).is_some(), "missing {name}");
    }

    // Negative control: the same session's model path really does ask the
    // provider for a count and omit the undiscovered deferred tools.
    let (model, _, complete, _) = fixture
        .orchestrator
        .build_wire_tools_with_complete()
        .await
        .unwrap();
    assert_eq!(fixture.api.0.load(Ordering::SeqCst), 1);
    assert!(!model.iter().any(|tool| tool.name == "TaskOutput"));
    assert!(!model.iter().any(|tool| tool.name == "TaskStop"));
    assert!(!model.iter().any(|tool| tool.name == "WaitForMcpServers"));
    fixture.tasks.assert_calls(&[]);
    for name in ["TaskOutput", "TaskStop"] {
        let listed = named(&catalog, name).unwrap();
        let definition = complete.iter().find(|tool| tool.name == name).unwrap();
        assert_eq!(
            listed
                .subprojection("/inputSchema")
                .unwrap()
                .to_json_string()
                .unwrap(),
            definition.input_schema.to_json_string().unwrap()
        );
        assert_eq!(
            listed.string_at("/description"),
            definition.description.string_at("")
        );
        let Some(ContentBlock::ToolResult {
            content, is_error, ..
        }) = call(&fixture.orchestrator, name).await
        else {
            panic!("missing result for {name}")
        };
        assert_ne!(is_error, Some(true), "{name} failed: {content:?}");
        let expected = if name == "TaskOutput" {
            "fixture output"
        } else {
            "Successfully stopped task"
        };
        assert!(content.to_json_string().unwrap().contains(expected));
        fixture.tasks.assert_calls(if name == "TaskOutput" {
            OUTPUT_CALLS
        } else {
            OUTPUT_AND_STOP_CALLS
        });
    }
    // TaskOutput checks existence and refreshes the local shell observation
    // around a single output read; TaskStop resolves the target before killing.
    assert_eq!(fixture.tasks.reads.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.tasks.outputs.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.tasks.stops.load(Ordering::SeqCst), 1);
    assert!(call(&fixture.orchestrator, "WaitForMcpServers")
        .await
        .is_some());
    assert_eq!(fixture.wait_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn host_catalog_and_dispatch_apply_enabled_capability_and_deny_filters() {
    let allowed = fixture(Some(&["TaskOutput"]), Arc::new(NoOpPermissionGate));
    let catalog = allowed.orchestrator.mcp_tool_definitions().await.unwrap();
    assert!(named(&catalog, "TaskOutput").is_some());
    assert!(named(&catalog, "TaskStop").is_none());
    assert!(call(&allowed.orchestrator, "TaskStop").await.is_none());
    allowed.tasks.assert_calls(&[]);
    assert!(call(&allowed.orchestrator, "TaskOutput").await.is_some());
    allowed.tasks.assert_calls(OUTPUT_CALLS);
    assert_eq!(allowed.tasks.reads.load(Ordering::SeqCst), 2);
    assert_eq!(allowed.tasks.outputs.load(Ordering::SeqCst), 1);
    assert_eq!(allowed.tasks.stops.load(Ordering::SeqCst), 0);

    let gate = Arc::new(permission::PolicyPermissionGate::new(
        Arc::new(permission::PermissionPolicy::from_rules(
            permission::PermissionMode::Default,
            vec![permission::PermissionRule {
                value: permission::PermissionRuleValue::from_rule_string("TaskStop"),
                behavior: permission::PermissionBehavior::Deny,
                source: permission::PermissionRuleSource::Session,
            }],
        )),
        Arc::new(NoOpPermissionGate),
    ));
    let denied = fixture(None, gate);
    assert!(named(
        &denied.orchestrator.mcp_tool_definitions().await.unwrap(),
        "TaskStop"
    )
    .is_none());
    assert!(call(&denied.orchestrator, "TaskStop").await.is_none());
    denied.tasks.assert_calls(&[]);
    assert!(call(&denied.orchestrator, "TaskOutput").await.is_some());
    denied.tasks.assert_calls(OUTPUT_CALLS);
    assert_eq!(denied.tasks.reads.load(Ordering::SeqCst), 2);
    assert_eq!(denied.tasks.outputs.load(Ordering::SeqCst), 1);
    assert_eq!(denied.tasks.stops.load(Ordering::SeqCst), 0);

    denied.wait_enabled.store(false, Ordering::SeqCst);
    assert!(named(
        &denied.orchestrator.mcp_tool_definitions().await.unwrap(),
        "WaitForMcpServers"
    )
    .is_none());
    assert!(call(&denied.orchestrator, "WaitForMcpServers")
        .await
        .is_none());
    assert_eq!(denied.wait_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn host_listing_does_not_replace_model_deferral_state() {
    let fixture = fixture(None, Arc::new(NoOpPermissionGate));
    fixture
        .orchestrator
        .tools
        .deferral()
        .set_mod_overrides(std::collections::HashMap::from([(
            "TaskStop".to_owned(),
            false,
        )]));
    assert!(!fixture
        .orchestrator
        .tools
        .deferral()
        .wants_defer("TaskStop", true));
    fixture.orchestrator.mcp_tool_definitions().await.unwrap();
    assert!(!fixture
        .orchestrator
        .tools
        .deferral()
        .wants_defer("TaskStop", true));
}

#[tokio::test]
async fn host_preparation_binds_builtin_identity_and_rejects_replacement() {
    fn marker(name: &'static str, calls: Arc<AtomicUsize>) -> Arc<dyn Tool> {
        Arc::new(MarkerTool {
            name,
            enabled: Arc::new(AtomicBool::new(true)),
            calls,
            validation_gate: None,
            gate_entered: AtomicBool::new(false),
        })
    }
    fn orchestrator(registry: Arc<ToolRegistry>) -> ConversationOrchestrator {
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(CountingApi::default()),
            registry,
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::with_files(Vec::new())),
            PathBuf::from("/work/repo"),
        )
    }

    let mut registry = ToolRegistry::new();
    registry.register_builtin(marker("Bash", Arc::new(AtomicUsize::new(0))));
    let builtin = orchestrator(Arc::new(registry));
    let prepared = builtin.prepare_host_tool_call("Bash").unwrap();
    assert!(prepared.supports_owned_process_cancellation());
    assert!(prepared.supports_explicit_background_handoff());

    let old_calls = Arc::new(AtomicUsize::new(0));
    let new_calls = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register_builtin(marker("Bash", Arc::new(AtomicUsize::new(0))));
    registry.set_builtin_catalog(&[]);
    let connection = lingxi_core::types::McpConnectionId::new();
    registry.register_mcp_tools(connection, vec![marker("Bash", old_calls.clone())]);
    let registry = Arc::new(registry);
    let orch = orchestrator(registry.clone());
    let original = orch.prepare_host_tool_call("Bash").unwrap();
    assert!(!original.supports_owned_process_cancellation());
    assert!(!original.supports_explicit_background_handoff());
    registry.register_mcp_tools(connection, vec![marker("Bash", new_calls.clone())]);
    assert!(orch
        .call_prepared_tool_from_host(
            original,
            ToolUseId::new(),
            generated_builtin_result(&json!({})),
            None,
            tool_api::context::HostToolCancellation::Ordinary,
        )
        .await
        .unwrap()
        .is_none());
    assert_eq!(old_calls.load(Ordering::SeqCst), 0);
    assert_eq!(new_calls.load(Ordering::SeqCst), 0);
    let replacement = orch.prepare_host_tool_call("Bash").unwrap();
    assert!(!replacement.supports_owned_process_cancellation());
    assert!(orch
        .call_prepared_tool_from_host(
            replacement,
            ToolUseId::new(),
            generated_builtin_result(&json!({})),
            None,
            tool_api::context::HostToolCancellation::Ordinary,
        )
        .await
        .unwrap()
        .is_some());
    assert_eq!(new_calls.load(Ordering::SeqCst), 1);
    let replacement = orch.prepare_host_tool_call("Bash").unwrap();
    assert!(orch
        .call_prepared_tool_from_host(
            replacement,
            ToolUseId::new(),
            generated_builtin_result(&json!({})),
            None,
            tool_api::context::HostToolCancellation::TerminateOwnedProcess(Arc::new(
                tool_api::context::HostToolRequest::default(),
            )),
        )
        .await
        .is_err());
    assert_eq!(new_calls.load(Ordering::SeqCst), 1);

    // A replacement after dispatch starts must not redirect the admitted call
    // to a different implementation. Pause inside the original validation so
    // the registry change deterministically happens after the policy recheck.
    let old_calls = Arc::new(AtomicUsize::new(0));
    let new_calls = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Barrier::new(2));
    registry.register_mcp_tools(
        connection,
        vec![Arc::new(MarkerTool {
            name: "Bash",
            enabled: Arc::new(AtomicBool::new(true)),
            calls: old_calls.clone(),
            validation_gate: Some(gate.clone()),
            gate_entered: AtomicBool::new(false),
        })],
    );
    let prepared = orch.prepare_host_tool_call("Bash").unwrap();
    let orch = Arc::new(orch);
    let task = tokio::spawn({
        let orch = orch.clone();
        async move {
            orch.call_prepared_tool_from_host(
                prepared,
                ToolUseId::new(),
                generated_builtin_result(&json!({})),
                None,
                tool_api::context::HostToolCancellation::Ordinary,
            )
            .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), gate.wait())
        .await
        .expect("original tool validation must start");
    registry.register_mcp_tools(connection, vec![marker("Bash", new_calls.clone())]);
    gate.wait().await;
    assert!(task.await.unwrap().unwrap().is_some());
    assert_eq!(old_calls.load(Ordering::SeqCst), 1);
    assert_eq!(new_calls.load(Ordering::SeqCst), 0);
}

struct Coordinator;

impl lingxi_core::host::coordinator_mode::CoordinatorModeHandle for Coordinator {
    fn is_enabled(&self) -> bool {
        true
    }
}

#[tokio::test]
async fn host_dispatch_refreshes_active_agent_restrictions_before_listing() {
    let fixture = fixture(None, Arc::new(NoOpPermissionGate));
    fixture
        .orchestrator
        .set_main_thread_agent(
            "restricted".to_owned(),
            None,
            agent::AgentToolPolicy::Explicit(vec![
                "TaskOutput".into(),
                "TaskStop".into(),
                "ToolSearch".into(),
                "WaitForMcpServers".into(),
            ]),
            vec!["TaskStop".into()],
            None,
        )
        .await;
    let model_tools = fixture.orchestrator.build_wire_tools().await.unwrap().0;
    assert!(!model_tools
        .iter()
        .any(|tool| tool.name == "WaitForMcpServers"));
    // A client is permitted to call without a previous tools/list request.
    assert!(call(&fixture.orchestrator, "TaskStop").await.is_none());
    fixture.tasks.assert_calls(&[]);
    assert!(call(&fixture.orchestrator, "TaskOutput").await.is_some());
    fixture.tasks.assert_calls(OUTPUT_CALLS);
    assert!(call(&fixture.orchestrator, "WaitForMcpServers")
        .await
        .is_some());
    let catalog = fixture.orchestrator.mcp_tool_definitions().await.unwrap();
    assert!(named(&catalog, "TaskOutput").is_some());
    assert!(named(&catalog, "TaskStop").is_none());
    assert!(named(&catalog, "WaitForMcpServers").is_some());
    fixture.tasks.assert_calls(OUTPUT_CALLS);
    assert_eq!(fixture.tasks.reads.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.tasks.outputs.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.tasks.stops.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.wait_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn model_wait_gate_precedes_agent_policy_without_changing_host_catalog() {
    let policies = [
        agent::AgentToolPolicy::Except(vec!["ToolSearch".into()]),
        agent::AgentToolPolicy::Explicit(vec!["WaitForMcpServers".into()]),
    ];
    for policy in policies {
        let fixture = fixture(None, Arc::new(NoOpPermissionGate));
        fixture
            .orchestrator
            .set_main_thread_agent("restricted".to_owned(), None, policy, vec![], None)
            .await;
        let model = fixture.orchestrator.build_wire_tools().await.unwrap().0;
        assert!(!model.iter().any(|tool| tool.name == "ToolSearch"));
        assert!(!model.iter().any(|tool| tool.name == "WaitForMcpServers"));
        let catalog = fixture.orchestrator.mcp_tool_definitions().await.unwrap();
        assert!(named(&catalog, "WaitForMcpServers").is_some());
        assert!(fixture
            .orchestrator
            .find_tool_for_dispatch("WaitForMcpServers")
            .is_none());
        assert!(call(&fixture.orchestrator, "WaitForMcpServers")
            .await
            .is_some());
        assert!(fixture
            .orchestrator
            .find_tool_for_dispatch("WaitForMcpServers")
            .is_none());
        let model = fixture.orchestrator.build_wire_tools().await.unwrap().0;
        assert!(!model.iter().any(|tool| tool.name == "WaitForMcpServers"));
    }
}

#[tokio::test]
async fn host_catalog_and_dispatch_preserve_coordinator_pool() {
    let fixture = fixture(None, Arc::new(NoOpPermissionGate));
    let orch = fixture
        .orchestrator
        .with_coordinator_mode(Arc::new(Coordinator))
        .with_coordinator_pool_for_test(false, &[]);
    let catalog = orch.mcp_tool_definitions().await.unwrap();
    assert!(named(&catalog, "TaskStop").is_some());
    assert!(named(&catalog, "TaskOutput").is_none());
    assert!(call(&orch, "TaskOutput").await.is_none());
    fixture.tasks.assert_calls(&[]);
    assert!(call(&orch, "TaskStop").await.is_some());
    fixture
        .tasks
        .assert_calls(&[("get", "b12345678"), ("kill", "b12345678")]);
    assert_eq!(fixture.tasks.reads.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.tasks.stops.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.tasks.outputs.load(Ordering::SeqCst), 0);
}
