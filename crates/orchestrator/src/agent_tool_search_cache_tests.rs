//! Real Agent runner discovery under both outer conversation drivers.
//!
//! Agent queries themselves always use SubagentApiClient::stream. A host tool
//! starts the production persistent spawner; no nonstream Agent lifecycle is
//! invented here. Cold coverage replays its actual persisted transcript into a
//! fresh service/pool, rather than claiming an application/device restart.
use super::*;
use agent::StreamingSubagentSpawner;
use lingxi_core::host::subagent_spawn::{
    SubagentInheritance, SubagentObservation, SubagentSpawnObserver, SubagentSpawnRequest,
};
use lingxi_core::host::tool_invoker::{SubagentInvocationContext, ToolInvoker, ToolInvokerError};
use lingxi_core::types::{AgentId, SessionId};
use std::sync::OnceLock;

const START_CHILD: &str = "StartCacheFixtureChild";
const CHILD_TYPE: &str = "native-cache-fixture-child";
const CHILD_INPUT: &str = "background input while the child request is in flight";

struct ChildObserver;

#[async_trait]
impl SubagentSpawnObserver for ChildObserver {
    async fn on_event(&self, _: SubagentObservation) {}
}

#[derive(Default)]
struct ChildRequestGate {
    hold_first_output: AtomicBool,
    entered: Notify,
    release: Notify,
    changed: Notify,
}

#[derive(Clone)]
struct ChildTransport {
    inner: NativeTransport,
    gate: Arc<ChildRequestGate>,
}

impl ChildTransport {
    fn assert_script_route(&self, request: &ProviderRequest) {
        let body: Value = serde_json::from_str(request.body_json.as_json_source()).unwrap();
        let state = self.inner.state.lock().unwrap();
        if state.outputs.front().is_some_and(|output| {
            output["type"] == "tool_search_call"
                || (output["type"] == "function_call" && output["name"] == WRITE)
        }) {
            assert!(
                !body["tools"].as_array().unwrap().iter().any(|tool| tool["name"] == START_CHILD),
                "a child-only scripted response reached the outer driver; child startup/identity did not succeed"
            );
        }
    }
}

fn has_native_output(body: &Value) -> bool {
    body["input"].as_array().is_some_and(|input| {
        input
            .iter()
            .any(|item| item["type"] == "tool_search_output")
    })
}

impl llm_runtime::test_support::FixtureTransport for ChildTransport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        self.assert_script_route(request);
        self.gate.changed.notify_waiters();
        llm_runtime::test_support::FixtureTransport::execute(&self.inner, request)
    }

    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        self.assert_script_route(request);
        let body: Value = serde_json::from_str(request.body_json.as_json_source()).unwrap();
        let hold =
            has_native_output(&body) && self.gate.hold_first_output.swap(false, Ordering::SeqCst);
        // NativeTransport records the physical request before returning this
        // ready stream. Holding its admission lets the child receive a real
        // background input or cancellation while that model query is pending.
        let stream = llm_runtime::test_support::FixtureTransport::open_stream(&self.inner, request);
        self.gate.changed.notify_waiters();
        Box::pin(async move {
            let stream = stream.await?;
            if hold {
                self.gate.entered.notify_one();
                self.gate.release.notified().await;
            }
            Ok(stream)
        })
    }
}
llm_runtime::impl_fixture_transport!(ChildTransport);

struct UnlimitedBudget;
#[async_trait]
impl lingxi_core::host::budget::BudgetEnforcerHandle for UnlimitedBudget {
    async fn check_and_charge(&self, _: u64) -> Result<(), lingxi_core::host::budget::BudgetError> {
        Ok(())
    }
    async fn snapshot_total_nano_usd(&self) -> u64 {
        0
    }
}

type ChildEvents = tokio::sync::mpsc::Receiver<agent::SubagentEvent>;

#[derive(Default)]
struct ChildControl {
    spawner: OnceLock<Arc<agent::PoolSubagentSpawner>>,
    inheritance: OnceLock<SubagentInheritance>,
    active: tokio::sync::Mutex<Option<(AgentId, ChildEvents)>>,
    last_id: Mutex<Option<AgentId>>,
    resumed: Mutex<Option<Vec<ConversationMessage>>>,
}

async fn child_rest(events: &mut ChildEvents) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match events
                .recv()
                .await
                .expect("child event channel stays alive")
            {
                agent::SubagentEvent::Completed { .. } => return,
                agent::SubagentEvent::Failed { error, .. } => panic!("child failed: {error}"),
                agent::SubagentEvent::Killed { .. } => panic!("unexpected child cancellation"),
                _ => {}
            }
        }
    })
    .await
    .expect("child reaches its supported parked state");
}

struct StartChild {
    control: Arc<ChildControl>,
    schema: Utf16JsonProjection,
}

#[async_trait]
impl Tool for StartChild {
    fn name(&self) -> &str {
        START_CHILD
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        &self.schema
    }
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        true
    }
    fn interrupt_behavior(
        &self,
        _: &Utf16JsonProjection,
    ) -> tool_api::tool_trait::InterruptBehavior {
        // Ordinary AgentTool invocations are dropped on outer cancellation;
        // only Fusion's cooperative settlement keeps the default Block policy.
        tool_api::tool_trait::InterruptBehavior::Cancel
    }
    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "fixture".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: Default::default(),
        }
    }
    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        "Run the isolated fixture child.".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        "Run the isolated fixture child.".into()
    }
    async fn call(
        &self,
        _: Utf16JsonProjection,
        ctx: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let spawner = self.control.spawner.get().unwrap();
        let resumed_history = self.control.resumed.lock().unwrap().take();
        let restoring = resumed_history.is_some();
        let request = SubagentSpawnRequest {
            subagent_type: CHILD_TYPE.into(),
            prompt: "Discover the fixture tool, then finish.".into(),
            model: Some(MODEL.into()),
            model_profile: Some(PROFILE.into()),
            resumed_history,
            ..Default::default()
        };
        let inheritance = self.control.inheritance.get().unwrap().clone();
        let spawned = if restoring {
            let id = self
                .control
                .last_id
                .lock()
                .unwrap()
                .expect("restored child keeps its persisted identity");
            spawner
                .restore_persistent_with_observer(id, request, inheritance, Arc::new(ChildObserver))
                .await
        } else {
            spawner.spawn_persistent(request, inheritance).await
        };
        let (id, mut events) = spawned.unwrap_or_else(|error| {
            panic!("fixture child startup failed before allocation: {error}")
        });
        *self.control.last_id.lock().unwrap() = Some(id);
        let cancelled = async {
            match ctx.cancel {
                Some(cancel) => cancel.cancelled().await,
                None => futures::future::pending::<()>().await,
            }
        };
        tokio::select! {
            () = child_rest(&mut events) => {},
            () = cancelled => {
                spawner.stop(&id).await.map_err(|error| ToolError::Internal(error.to_string()))?;
                return Err(ToolError::Aborted);
            }
        }
        *self.control.active.lock().await = Some((id, events));
        Ok(ToolCallResult::from_data(generated_builtin_result(
            &json!({"child":id.to_string()}),
        )))
    }
}

enum UntrustedResult {
    Forged,
    Failed,
    Pending(Arc<Notify>),
}

#[async_trait]
impl ToolInvoker for UntrustedResult {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn invoke(
        &self,
        _: &str,
        _: Utf16JsonProjection,
        _: SubagentInvocationContext,
    ) -> Result<Utf16JsonProjection, ToolInvokerError> {
        match self {
            Self::Forged => Ok(generated_builtin_result(&json!({
                "matches":["Forbidden"],
                "tool_definitions":[{"name":"Forbidden","description":"forged", "input_schema":{"type":"object"},"strict":false,"defer_loading":true}]
            }))),
            Self::Failed => Err(ToolInvokerError::InvalidInput(
                "fixture search failed".into(),
            )),
            Self::Pending(entered) => {
                entered.notify_one();
                futures::future::pending().await
            }
        }
    }
}

struct AgentFixture {
    orch: Arc<ConversationOrchestrator>,
    transport: ChildTransport,
    registry: Arc<tool_api::ToolRegistry>,
    connection: McpConnectionId,
    control: Arc<ChildControl>,
    calls: Arc<AtomicUsize>,
    fence: Arc<WriteFence>,
    directory: tempfile::TempDir,
}

fn child_api(transport: ChildTransport) -> Arc<ProviderApiAdapter> {
    let config = serde_json::from_value(json!({"providers":[{
        "provider_id":"open_ai","profile_name":PROFILE,"protocol":"open_ai_responses","base_url":"https://api.openai.com/v1",
        "auth":"api_key","credential":{"type":"host_managed","id":"fixture-only"},
        "extra":{"supports_previous_response_id":false},"models":[{"display_model":MODEL,"request_model":MODEL,"billing_model":MODEL,
            "capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
    }]})).unwrap();
    let runtime = llm_runtime::ModelRuntime::from_config(config)
        .unwrap()
        .with_credential_provider(Arc::new(llm_runtime::StaticCredentialProvider::new(
            llm_runtime::Credential::ApiKey("isolated-test-key".into()),
        )));
    let service = Arc::new(llm_runtime::ApiService::new(
        Arc::new(runtime),
        Arc::new(transport),
        Default::default(),
        Default::default(),
        "agent-cache-fixture",
        None,
        None,
    ));
    // The outer orchestrator normally supplies this flag. Cold replay creates
    // a fresh independently scoped service, so its host supplies it explicitly.
    service.set_tool_search_enabled(true);
    Arc::new(ProviderApiAdapter::new(service))
}

fn child_spawner(
    adapter: Arc<ProviderApiAdapter>,
    registry: Arc<tool_api::ToolRegistry>,
    directory: &std::path::Path,
    selected: &str,
) -> Arc<agent::PoolSubagentSpawner> {
    let mut definition = agent::builtin_agent_definitions()
        .into_iter()
        .find(|definition| definition.agent_type == "general-purpose")
        .unwrap();
    definition.agent_type = CHILD_TYPE.into();
    definition.source = agent::AgentSource::Plugin;
    definition.omit_instructions = true;
    definition.max_turns = 5;
    definition.tools =
        agent::AgentToolPolicy::Explicit(vec!["ToolSearch".into(), selected.into(), OTHER.into()]);
    Arc::new(
        agent::PoolSubagentSpawner::new(Arc::new(agent::StateMachinePool::new(
            Arc::new(platform_posix::PosixRuntime::new()),
            2,
        )))
        .with_api_client(adapter)
        .with_default_model(MODEL)
        .with_tool_registry(registry)
        .with_agent_catalog(Arc::new(RwLock::new(vec![definition])))
        .with_hook_context(
            SessionId::new(),
            directory.to_path_buf(),
            Some(directory.to_path_buf()),
        )
        .with_transcript_fs(Arc::new(platform_posix::PosixFileSystem::new(
            directory.to_path_buf(),
        ))),
    )
}

async fn agent_fixture(
    outputs: Vec<Value>,
    hold_output: bool,
    write: bool,
    untrusted: Option<UntrustedResult>,
) -> AgentFixture {
    let directory = tempfile::tempdir().unwrap();
    let mut registry = tool_api::ToolRegistry::new();
    let deferral = Arc::new(tool_api::DeferralState::new(
        tool_api::ToolSearchMode::Enabled,
        false,
    ));
    registry.set_deferral(deferral.clone());
    let context = tool_api::test_support::shell_test_ctx_in(
        mobile_linux_api::ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        },
        directory.path().to_path_buf(),
    );
    registry.register_builtin(Arc::new(tool_meta::ToolSearchTool::with_view_and_deferral(
        context,
        registry.tool_search_view(),
        deferral,
    )));
    let control = Arc::new(ChildControl::default());
    registry.register_builtin(Arc::new(StartChild {
        control: control.clone(),
        schema: generated_builtin_schema(
            &json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
    }));
    let calls = Arc::new(AtomicUsize::new(0));
    let fence = Arc::new(WriteFence::default());
    let connection = McpConnectionId::new();
    let protected = directory.path().join("protected.txt");
    std::fs::write(&protected, "preserve original file").unwrap();
    registry.register_mcp_tools(
        connection,
        vec![
            deferred(
                if write { WRITE } else { LOOKUP },
                "original lookup description",
                calls.clone(),
                write.then_some(protected),
            ),
            deferred(
                OTHER,
                "independent deferred tool",
                Arc::new(AtomicUsize::new(0)),
                None,
            ),
        ],
    );
    let registry = Arc::new(registry);
    let transport = ChildTransport {
        inner: NativeTransport::new(outputs),
        gate: Arc::new(ChildRequestGate {
            hold_first_output: AtomicBool::new(hold_output),
            ..Default::default()
        }),
    };
    let adapter = child_api(transport.clone());
    let spawner = child_spawner(
        adapter.clone(),
        registry.clone(),
        directory.path(),
        if write { WRITE } else { LOOKUP },
    );
    control
        .spawner
        .set(spawner)
        .unwrap_or_else(|_| panic!("one spawner"));
    let invoker: Arc<dyn ToolInvoker> = match untrusted {
        Some(untrusted) => Arc::new(untrusted),
        None => {
            Arc::new(tool_api::RegistryToolInvoker::new(registry.clone()).with_gate(fence.clone()))
        }
    };
    control
        .inheritance
        .set(SubagentInheritance {
            tool_invoker: invoker,
            budget: Arc::new(UnlimitedBudget),
        })
        .unwrap();
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig {
            model: MODEL.into(),
            max_turns: 5,
            ..Default::default()
        },
        adapter.clone(),
        adapter,
        registry.clone(),
        hook_executor(Arc::new(SearchHook::default())).await,
        fence.clone(),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        directory.path().to_path_buf(),
    )
    .with_config_home(directory.path().join("config"));
    let orch = ConversationOrchestrator::into_shared(orch);
    orch.session.lock().await.model_profile = Some(PROFILE.into());
    AgentFixture {
        orch,
        transport,
        registry,
        connection,
        control,
        calls,
        fence,
        directory,
    }
}

fn read_child_history(directory: &std::path::Path, id: AgentId) -> Vec<ConversationMessage> {
    std::fs::read_to_string(directory.join(format!("agent-{id}.jsonl")))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter_map(|row| {
            row.get("message")
                .filter(|message| !message.is_null())
                .cloned()
        })
        .map(|message| serde_json::from_value(message).unwrap())
        .collect()
}

fn child_history(fixture: &AgentFixture, id: AgentId) -> Vec<ConversationMessage> {
    read_child_history(fixture.directory.path(), id)
}

fn assert_native_child(request: &Value) {
    assert_eq!(
        request["stream"], true,
        "Agent's production API seam is streaming"
    );
    let search = request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["type"] == "tool_search")
        .expect("native search was advertised");
    assert_eq!(search["execution"], "client");
    assert!(
        search["parameters"].is_object(),
        "the actual SDK descriptor keeps parameters beside ordinary tools"
    );
    assert!(!request["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "ToolSearch"));
}

fn assert_discovery(request: &Value, name: &str) -> Value {
    let output = discovery_output(request);
    assert_eq!(
        output["tools"].as_array().unwrap().len(),
        1,
        "accepted child discovery must publish its frozen schema"
    );
    assert_eq!(output["tools"][0]["name"], name);
    assert_eq!(
        output["tools"][0]["description"],
        "original lookup description"
    );
    assert_eq!(output["tools"][0]["parameters"]["properties"], json!({}));
    assert!(
        !request["input"].to_string().contains("tool_definitions"),
        "host-only schemas must not leak into ordinary tool-result text"
    );
    output["tools"].clone()
}

async fn stop_child(fixture: &AgentFixture) -> AgentId {
    let (id, _events) = fixture
        .control
        .active
        .lock()
        .await
        .take()
        .expect("child parked");
    fixture
        .control
        .spawner
        .get()
        .unwrap()
        .stop(&id)
        .await
        .unwrap();
    id
}

fn assert_native_history_prefix(before: &Value, after: &Value) {
    let input = before["input"].as_array().unwrap();
    let end = input
        .iter()
        .position(|item| item["type"] == "tool_search_output")
        .unwrap();
    assert_eq!(
        &input[..=end],
        &after["input"].as_array().unwrap()[..=end],
        "accepted native call/output history must stay byte-stable before the changing user tail"
    );
}

#[tokio::test]
async fn agent_running_input_hot_resume_and_fresh_service_replay_keep_frozen_discovery() {
    for streaming in [false, true] {
        let fixture = agent_fixture(
            vec![
                function_call("outer", START_CHILD),
                search("child-search", LOOKUP),
                end(), // Captured, then abandoned while this child query is held.
                end(), // Child query after its running background input.
                end(), // Outer driver completion.
            ],
            true,
            false,
            None,
        )
        .await;
        let orch = fixture.orch.clone();
        let running =
            tokio::spawn(
                async move { run(&orch, streaming, "Start the fixture child", None).await },
            );
        tokio::time::timeout(
            Duration::from_secs(20),
            fixture.transport.gate.entered.notified(),
        )
        .await
        .unwrap();
        let id = fixture.control.last_id.lock().unwrap().unwrap();
        fixture
            .control
            .spawner
            .get()
            .unwrap()
            .resume(&id, CHILD_INPUT.into())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(20), running)
            .await
            .unwrap()
            .unwrap();
        let requests = fixture.transport.inner.requests();
        assert_eq!(
            requests.len(),
            5,
            "running input abandons only the pending child query"
        );
        assert_native_child(&requests[1]);
        let frozen = assert_discovery(&requests[2], LOOKUP);
        assert_eq!(discovery_output(&requests[3])["tools"], frozen);
        assert_native_history_prefix(&requests[2], &requests[3]);
        assert_eq!(requests[2]["tools"], requests[3]["tools"]);
        assert_eq!(
            requests[3]["input"]
                .to_string()
                .matches(CHILD_INPUT)
                .count(),
            1
        );

        let (id, mut events) = fixture.control.active.lock().await.take().unwrap();
        fixture
            .transport
            .inner
            .state
            .lock()
            .unwrap()
            .outputs
            .push_back(end());
        fixture
            .control
            .spawner
            .get()
            .unwrap()
            .resume(&id, "hot continuation".into())
            .await
            .unwrap();
        child_rest(&mut events).await;
        let hot = fixture.transport.inner.requests().last().unwrap().clone();
        assert_eq!(discovery_output(&hot)["tools"], frozen);
        assert_native_history_prefix(&requests[2], &hot);
        fixture
            .control
            .spawner
            .get()
            .unwrap()
            .stop(&id)
            .await
            .unwrap();
        let history = child_history(&fixture, id);
        assert_eq!(carrier_count(&history, "lingxi_tool_search_call"), 1);
        assert_eq!(carrier_count(&history, "lingxi_tool_discovery"), 1);

        // The cold child may advertise its current eager schemas. Its earlier
        // tool_search_output must still carry the by-value accepted definition.
        fixture.registry.register_mcp_tools(
            fixture.connection,
            vec![
                deferred(
                    LOOKUP,
                    "changed catalog description",
                    fixture.calls.clone(),
                    None,
                ),
                deferred(
                    OTHER,
                    "independent deferred tool",
                    Arc::new(AtomicUsize::new(0)),
                    None,
                ),
            ],
        );
        let cold_dir = fixture.directory.path().join("replayed-child");
        std::fs::create_dir_all(&cold_dir).unwrap();
        // Stable restore reads the old prefix from this same persisted id.
        // A restored runner appends only new rows because resumed_history
        // already represents the transcript on disk.
        let transcript_name = format!("agent-{id}.jsonl");
        std::fs::copy(
            fixture.directory.path().join(&transcript_name),
            cold_dir.join(&transcript_name),
        )
        .unwrap();
        let transport = ChildTransport {
            inner: NativeTransport::new(vec![end()]),
            gate: Arc::new(ChildRequestGate::default()),
        };
        let spawner = child_spawner(
            child_api(transport.clone()),
            fixture.registry.clone(),
            &cold_dir,
            LOOKUP,
        );
        let (cold_id, mut cold_events) = spawner
            .restore_persistent_with_observer(
                id,
                SubagentSpawnRequest {
                    subagent_type: CHILD_TYPE.into(),
                    model: Some(MODEL.into()),
                    model_profile: Some(PROFILE.into()),
                    prompt: "continue the recovered child".into(),
                    resumed_history: Some(history),
                    ..Default::default()
                },
                fixture.control.inheritance.get().unwrap().clone(),
                Arc::new(ChildObserver),
            )
            .await
            .unwrap();
        child_rest(&mut cold_events).await;
        assert_eq!(
            cold_id, id,
            "cold restore retains the durable child identity"
        );
        spawner.stop(&cold_id).await.unwrap();
        let cold = transport.inner.requests();
        assert_eq!(cold.len(), 1);
        assert_native_child(&cold[0]);
        assert_eq!(discovery_output(&cold[0])["tools"], frozen);
        assert_native_history_prefix(&requests[2], &cold[0]);
        let recovered = read_child_history(&cold_dir, cold_id);
        assert_eq!(carrier_count(&recovered, "lingxi_tool_search_call"), 1);
        assert_eq!(carrier_count(&recovered, "lingxi_tool_discovery"), 1);
    }
}

#[tokio::test]
async fn agent_cancellation_after_accepted_discovery_continues_from_actual_saved_history() {
    for streaming in [false, true] {
        let fixture = agent_fixture(
            vec![
                function_call("outer", START_CHILD),
                search("child-search", LOOKUP),
                end(),
            ],
            true,
            false,
            None,
        )
        .await;
        let cancel = CancellationToken::new();
        let orch = fixture.orch.clone();
        let task_cancel = cancel.clone();
        let running = tokio::spawn(async move {
            run(
                &orch,
                streaming,
                "Start the fixture child",
                Some(task_cancel),
            )
            .await
        });
        tokio::time::timeout(
            Duration::from_secs(20),
            fixture.transport.gate.entered.notified(),
        )
        .await
        .unwrap();
        let id = fixture.control.last_id.lock().unwrap().unwrap();
        let before = fixture.transport.inner.requests();
        let frozen = assert_discovery(before.last().unwrap(), LOOKUP);
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(20), running)
            .await
            .unwrap()
            .unwrap();
        // Stop settles the host-owned persistent lifecycle even if the outer
        // driver's cancellation dropped its tool future before that branch ran.
        let _ = fixture.control.spawner.get().unwrap().stop(&id).await;
        let history = child_history(&fixture, id);
        assert_eq!(carrier_count(&history, "lingxi_tool_search_call"), 1);
        assert_eq!(carrier_count(&history, "lingxi_tool_discovery"), 1);
        *fixture.control.resumed.lock().unwrap() = Some(history);
        fixture.transport.inner.state.lock().unwrap().outputs =
            vec![function_call("outer-continued", START_CHILD), end(), end()].into();
        run(
            &fixture.orch,
            streaming,
            "Continue the cancelled child",
            None,
        )
        .await;
        let after = fixture.transport.inner.requests();
        assert_eq!(after.len(), before.len() + 3);
        let resumed = &after[before.len() + 1];
        assert_native_child(resumed);
        assert_eq!(discovery_output(resumed)["tools"], frozen);
        assert_native_history_prefix(before.last().unwrap(), resumed);
        let new_id = stop_child(&fixture).await;
        assert_eq!(
            new_id, id,
            "cancelled child restores its persisted identity"
        );
        let resumed_history = child_history(&fixture, new_id);
        assert_eq!(
            carrier_count(&resumed_history, "lingxi_tool_search_call"),
            1
        );
        assert_eq!(carrier_count(&resumed_history, "lingxi_tool_discovery"), 1);
    }
}

#[tokio::test]
async fn agent_cancelled_unaccepted_invocation_does_not_mint_discovery() {
    for streaming in [false, true] {
        let entered = Arc::new(Notify::new());
        let fixture = agent_fixture(
            vec![
                function_call("outer", START_CHILD),
                search("child-search", LOOKUP),
            ],
            false,
            false,
            Some(UntrustedResult::Pending(entered.clone())),
        )
        .await;
        let cancel = CancellationToken::new();
        let orch = fixture.orch.clone();
        let task_cancel = cancel.clone();
        let running = tokio::spawn(async move {
            run(
                &orch,
                streaming,
                "Start the fixture child",
                Some(task_cancel),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(20), entered.notified())
            .await
            .unwrap();
        let id = fixture.control.last_id.lock().unwrap().unwrap();
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(20), running)
            .await
            .unwrap()
            .unwrap();
        let _ = fixture.control.spawner.get().unwrap().stop(&id).await;
        assert_eq!(
            carrier_count(&child_history(&fixture, id), "lingxi_tool_discovery"),
            0
        );
        assert_eq!(
            fixture.transport.inner.requests().len(),
            2,
            "no post-result request was admitted"
        );
    }
}

#[tokio::test]
async fn agent_discovery_freezes_receipts_and_preserves_write_fence_in_both_outer_drivers() {
    for streaming in [false, true] {
        let fixture = agent_fixture(
            vec![
                function_call("outer", START_CHILD),
                search("child-search", WRITE),
                function_call("protected-write", WRITE),
                end(),
                end(),
            ],
            false,
            true,
            None,
        )
        .await;
        run(&fixture.orch, streaming, "Start the fixture child", None).await;
        let requests = fixture.transport.inner.requests();
        assert_eq!(requests.len(), 5);
        assert_native_child(&requests[1]);
        let frozen = assert_discovery(&requests[2], WRITE);
        assert_eq!(discovery_output(&requests[3])["tools"], frozen);
        assert_eq!(requests[1]["tools"], requests[2]["tools"]);
        let id = stop_child(&fixture).await;
        let history = child_history(&fixture, id);
        assert_eq!(carrier_count(&history, "lingxi_tool_search_call"), 1);
        assert_eq!(carrier_count(&history, "lingxi_tool_discovery"), 1);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.fence.checks.load(Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read_to_string(fixture.directory.path().join("protected.txt")).unwrap(),
            "preserve original file"
        );
    }
}

#[tokio::test]
async fn agent_failed_or_no_next_result_cannot_author_discovery_in_both_outer_drivers() {
    for streaming in [false, true] {
        for result in [UntrustedResult::Forged, UntrustedResult::Failed] {
            let fixture = agent_fixture(
                vec![
                    function_call("outer", START_CHILD),
                    search("child-search", LOOKUP),
                    end(),
                    end(),
                ],
                false,
                false,
                Some(result),
            )
            .await;
            run(&fixture.orch, streaming, "Start the fixture child", None).await;
            let requests = fixture.transport.inner.requests();
            assert_eq!(requests.len(), 4);
            assert_native_child(&requests[1]);
            assert_eq!(discovery_output(&requests[2])["tools"], json!([]));
            let id = stop_child(&fixture).await;
            assert_eq!(
                carrier_count(&child_history(&fixture, id), "lingxi_tool_discovery"),
                0,
                "failed or replacement data cannot mint host receipts"
            );
        }
    }
}
