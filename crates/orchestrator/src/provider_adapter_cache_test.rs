//! End-to-end cache diagnostics through real request preparation and both loops.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use cost::prompt_cache_ledger::{CacheTtl, MissCause};
use llm_runtime::{
    BoxFuture, FrameStream, LlmError, ProviderRequest, ProviderResponse, RawStreamFrame,
    StreamingResponse,
};
use serde_json::json;

use crate::test_support::{
    noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::{ConversationOrchestrator, OrchestratorConfig};

const MODEL: &str = "claude-sonnet-4-20250514";

struct Frames(VecDeque<RawStreamFrame>);

impl FrameStream for Frames {
    fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<RawStreamFrame>, LlmError>> {
        let next = self.0.pop_front();
        Box::pin(async move { Ok(next) })
    }
}

#[derive(Default)]
struct CacheTransport {
    seen: Arc<Mutex<Vec<ProviderRequest>>>,
    requests: AtomicUsize,
    fallback: bool,
}

impl CacheTransport {
    fn response_usage(&self) -> serde_json::Value {
        let mut value = usage();
        if self.requests.fetch_add(1, Ordering::SeqCst) == 1 {
            value["cache_read_input_tokens"] = json!(10000);
            value["cache_creation_input_tokens"] = json!(0);
            value["cache_creation"]["ephemeral_1h_input_tokens"] = json!(0);
        }
        value
    }
}

fn usage() -> serde_json::Value {
    json!({
        "input_tokens":100,"output_tokens":1,"cache_read_input_tokens":0,
        "cache_creation_input_tokens":10000,
        "cache_creation":{"ephemeral_1h_input_tokens":10000,"ephemeral_5m_input_tokens":0}
    })
}

impl llm_runtime::test_support::FixtureTransport for CacheTransport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        self.seen.lock().unwrap().push(request.clone());
        let usage = self.response_usage();
        Box::pin(async move {
            Ok(ProviderResponse::json(
                200,
                json!({
                    "id":"cache-response","model":MODEL,"content":[{"type":"text","text":"ok"}],
                    "stop_reason":"end_turn","usage":usage
                }),
            ))
        })
    }

    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        self.seen.lock().unwrap().push(request.clone());
        let usage = self.response_usage();
        let mut values = vec![
            json!({"type":"message_start","message":{"id":"cache-stream","model":MODEL,"content":[],"usage":usage}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":usage}),
            json!({"type":"message_stop"}),
        ];
        if self.fallback {
            values.truncate(1);
            values
                .push(json!({"type":"error","error":{"type":"overloaded_error","message":"busy"}}));
        }
        let frames = values
            .into_iter()
            .map(|value| RawStreamFrame::new(serde_json::to_vec(&value).unwrap()))
            .collect();
        Box::pin(async move {
            Ok(StreamingResponse {
                status: 200,
                headers: BTreeMap::new(),
                frames: Box::new(Frames(frames)),
            })
        })
    }
}

llm_runtime::impl_fixture_transport!(CacheTransport);

async fn orchestrator_with_transport(transport: CacheTransport) -> Arc<ConversationOrchestrator> {
    let adapter = Arc::new(super::tests::make_adapter(Arc::new(transport)));
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig {
            model: MODEL.into(),
            // This diagnostics fixture needs a mutable system source. Current
            // auxiliary requests skip the main session's frozen static slate.
            query_source: "auxiliary:cache_diagnostics".into(),
            ..Default::default()
        },
        adapter.clone(),
        adapter,
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));
    assert!(!orch.prompt_snapshot_eligible());
    orch.set_main_thread_agent(
        "cache-test".to_string(),
        Some("first system".to_string()),
        agent::AgentToolPolicy::All {
            use_exact_tools: false,
        },
        Vec::new(),
        None,
    )
    .await;
    orch
}

async fn run(orch: &ConversationOrchestrator, streaming: bool) {
    if streaming {
        orch.run_turn_streaming("continue").await.unwrap();
    } else {
        orch.run_turn("continue").await.unwrap();
    }
}

async fn assert_diagnostics(streaming: bool) {
    let transport = CacheTransport::default();
    let seen = transport.seen.clone();
    let orch = orchestrator_with_transport(transport).await;
    run(&orch, streaming).await;
    {
        let state = orch.current_prompt_cache_ledger().await;
        let summary = state.ledger.summary(0);
        assert_eq!(summary.requests, 1);
        assert_eq!(summary.last_request.unwrap().facts.ttl, CacheTtl::OneHour);
    }

    run(&orch, streaming).await;
    assert_eq!(
        orch.current_prompt_cache_ledger()
            .await
            .ledger
            .summary(0)
            .hits,
        1
    );
    orch.set_main_thread_agent(
        "cache-test".to_string(),
        Some("second system 😀".to_string()),
        agent::AgentToolPolicy::All {
            use_exact_tools: false,
        },
        Vec::new(),
        None,
    )
    .await;
    run(&orch, streaming).await;
    {
        let state = orch.current_prompt_cache_ledger().await;
        let summary = state.ledger.summary(0);
        assert_eq!(
            summary.requests, 3,
            "one entry per response, not per stream event"
        );
        assert_eq!(summary.misses, 1);
        let attribution = summary.last_miss_attribution.unwrap();
        assert!(attribution.causes.contains(&MissCause::SystemPromptChanged));
        // Non-ephemeral token reminders retain their original place in history.
        // Changing only the system prompt must not report a message rewrite.
        assert!(!attribution.causes.contains(&MissCause::MessagesRewritten));
        let requests = seen.lock().unwrap();
        let previous_user = &requests[1].body_json["messages"][2]["content"];
        let current_user = &requests[2].body_json["messages"][2]["content"];
        assert_eq!(previous_user[0]["text"], "continue\n");
        assert!(previous_user[1]["text"]
            .as_str()
            .unwrap()
            .starts_with("<system-reminder>\n<total_tokens>"));
        // The SDK moves the request-local checkpoint to the new user tail.
        // All prior model content stays byte-identical; only that checkpoint
        // is removed from the previous tail, as the diagnostics already model.
        let old_checkpoint = previous_user[1]["cache_control"].clone();
        assert_eq!(old_checkpoint, json!({"type":"ephemeral"}));
        assert!(current_user[1].get("cache_control").is_none());
        let mut prior_without_checkpoint = previous_user.clone();
        prior_without_checkpoint[1]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        assert_eq!(current_user, &prior_without_checkpoint);
        let new_tail = requests[2].body_json["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap();
        assert_eq!(
            new_tail["content"].as_array().unwrap().last().unwrap()["cache_control"],
            old_checkpoint,
        );
        assert_eq!(attribution.system_char_delta, Some(4));
    }

    orch.expect_prompt_cache_rebuild().await;
    assert_eq!(
        orch.current_prompt_cache_ledger()
            .await
            .ledger
            .estimate_recache_tokens(),
        None
    );
    run(&orch, streaming).await;
    let summary = orch.current_prompt_cache_ledger().await.ledger.summary(0);
    assert_eq!(
        (summary.requests, summary.misses, summary.expected_rebuilds),
        (4, 1, 1)
    );

    orch.session.lock().await.session_id = lingxi_core::types::SessionId::new();
    assert_eq!(
        orch.current_prompt_cache_ledger()
            .await
            .ledger
            .summary(0)
            .requests,
        0
    );
    run(&orch, streaming).await;
    let summary = orch.current_prompt_cache_ledger().await.ledger.summary(0);
    assert_eq!(
        (summary.requests, summary.cold_starts, summary.misses),
        (1, 1, 0)
    );
}

#[tokio::test]
async fn prompt_cache_batched_diagnostics_use_final_request_and_reported_ttl() {
    assert_diagnostics(false).await;
}

#[tokio::test]
async fn prompt_cache_streaming_diagnostics_use_final_request_and_reported_ttl() {
    assert_diagnostics(true).await;
}

#[tokio::test]
async fn prompt_cache_stream_fallback_retains_the_first_stream_cache_write() {
    let orch = orchestrator_with_transport(CacheTransport {
        fallback: true,
        ..Default::default()
    })
    .await;
    run(&orch, true).await;
    let state = orch.current_prompt_cache_ledger().await;
    let summary = state.ledger.summary(0);
    assert_eq!(
        (summary.requests, summary.cold_starts, summary.hits),
        (2, 1, 1)
    );
    assert_eq!(summary.cache_write_tokens, 10_000);
    assert_eq!(summary.last_request.unwrap().facts.ttl, CacheTtl::OneHour);
}

#[tokio::test]
async fn prompt_cache_cold_rewrite_is_a_miss_without_read_drop_attribution() {
    let orch = orchestrator_with_transport(CacheTransport {
        requests: AtomicUsize::new(2),
        ..Default::default()
    })
    .await;
    run(&orch, false).await;
    orch.set_main_thread_agent(
        "cache-test".to_string(),
        Some("changed system".to_string()),
        agent::AgentToolPolicy::All {
            use_exact_tools: false,
        },
        Vec::new(),
        None,
    )
    .await;
    run(&orch, false).await;
    let summary = orch.current_prompt_cache_ledger().await.ledger.summary(0);
    assert_eq!((summary.requests, summary.misses), (2, 1));
    assert_eq!(summary.last_miss_attribution, None);
}

struct ChildInstructionSource(Mutex<String>);

#[async_trait::async_trait]
impl lingxi_core::host::instructions::InstructionContextProvider for ChildInstructionSource {
    async fn load(
        &self,
        _: &std::path::Path,
        _: lingxi_core::host::instructions::InstructionScope,
    ) -> Result<lingxi_core::host::instructions::InstructionContext, String> {
        use lingxi_core::host::instructions::{
            InstructionContext, InstructionFile, InstructionFileType,
        };
        let body = self.0.lock().unwrap().clone();
        Ok(InstructionContext {
            user_context: BTreeMap::from([("instructions".into(), body.clone())]),
            eager_instructions: Some(vec![InstructionFile {
                path: "/managed/LINGXI.md".into(),
                kind: InstructionFileType::Managed,
                content: body,
            }]),
            ..Default::default()
        })
    }
}

struct ChildInstructionInheritance;

#[async_trait::async_trait]
impl lingxi_core::host::budget::BudgetEnforcerHandle for ChildInstructionInheritance {
    async fn check_and_charge(&self, _: u64) -> Result<(), lingxi_core::host::budget::BudgetError> {
        Ok(())
    }
    async fn snapshot_total_nano_usd(&self) -> u64 {
        0
    }
}

#[async_trait::async_trait]
impl lingxi_core::host::tool_invoker::ToolInvoker for ChildInstructionInheritance {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn invoke(
        &self,
        _: &str,
        _: serde_json::Value,
        _: lingxi_core::host::tool_invoker::SubagentInvocationContext,
    ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
        unreachable!("this fixture produces text responses only")
    }
}

fn prepared_child_text(request: &ProviderRequest) -> String {
    request.body_json["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().unwrap())
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

async fn child_reaches_rest(rx: &mut tokio::sync::mpsc::Receiver<agent::SubagentEvent>) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await.expect("persistent runner remains alive") {
                agent::SubagentEvent::Completed { .. } => return,
                agent::SubagentEvent::Failed { error, .. } => panic!("child failed: {error}"),
                _ => {}
            }
        }
    })
    .await
    .expect("child parks after the prepared request");
}

#[tokio::test]
async fn child_announcements_reach_prepared_wire_and_cold_resume_reloads_policy() {
    use agent::StreamingSubagentSpawner;
    use lingxi_core::host::instructions::InstructionContext;
    use lingxi_core::host::subagent_spawn::{
        SubagentInheritance, SubagentResult, SubagentSpawnRequest, SubagentSpawner,
    };
    let transport = CacheTransport::default();
    let seen = transport.seen.clone();
    let adapter = Arc::new(super::tests::make_adapter(Arc::new(transport)));
    let policy = Arc::new(ChildInstructionSource(Mutex::new(
        "INITIAL MANAGED POLICY".into(),
    )));
    let directory = tempfile::tempdir().unwrap();
    let mut definition = agent::builtin_agent_definitions()
        .into_iter()
        .find(|definition| definition.agent_type == "general-purpose")
        .unwrap();
    definition.agent_type = "policy-worker".into();
    definition.source = agent::AgentSource::Plugin;
    definition.omit_instructions = true;
    definition.max_turns = 1;
    let spawner = agent::PoolSubagentSpawner::new(Arc::new(agent::StateMachinePool::new(
        Arc::new(platform_posix::PosixRuntime::new()),
        2,
    )))
    .with_api_client(adapter)
    .with_default_model(MODEL)
    .with_instruction_provider(policy.clone())
    .with_agent_catalog(Arc::new(tokio::sync::RwLock::new(vec![definition])))
    .with_hook_context(
        lingxi_core::types::SessionId::new(),
        directory.path().to_path_buf(),
        Some(directory.path().to_path_buf()),
    )
    .with_transcript_fs(Arc::new(platform_posix::PosixFileSystem::new(
        directory.path().to_path_buf(),
    )));
    let inheritance = || SubagentInheritance {
        tool_invoker: Arc::new(ChildInstructionInheritance),
        budget: Arc::new(ChildInstructionInheritance),
    };
    let request = SubagentSpawnRequest {
        subagent_type: "policy-worker".into(),
        prompt: "check the policy".into(),
        ..Default::default()
    };
    let (id, mut events) = spawner
        .spawn_persistent(request.clone(), inheritance())
        .await
        .unwrap();
    child_reaches_rest(&mut events).await;
    *policy.0.lock().unwrap() = "CURRENT MANAGED POLICY".into();
    spawner.resume(&id, "continue".into()).await.unwrap();
    child_reaches_rest(&mut events).await;
    spawner.stop(&id).await.unwrap();
    {
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            let text = prepared_child_text(request);
            assert_eq!(text.matches("INITIAL MANAGED POLICY").count(), 1);
            assert!(!text.contains("CURRENT MANAGED POLICY"));
            assert!(!text.contains("# instructions"));
        }
    }
    let rows: Vec<serde_json::Value> =
        std::fs::read_to_string(directory.path().join(format!("agent-{id}.jsonl")))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
    assert!(!rows
        .iter()
        .any(|row| row["message"]["subtype"] == "instruction_context"));
    let initial: Vec<_> = rows
        .iter()
        .filter(|row| row["attachment"]["type"] == "instructions")
        .collect();
    assert_eq!(initial.len(), 1, "live resume retains its announced policy");
    assert_eq!(
        initial[0]["attachment"],
        json!({"type":"instructions","files":[{
            "path":"/managed/LINGXI.md","type":"Managed","content":"INITIAL MANAGED POLICY"
        }]})
    );
    // Restore from the actual saved rows, retaining only their typed attachment
    // baseline. Gv/userContext and Ye's lazy cursor are private process state.
    let mut resumed: SubagentSpawnRequest =
        serde_json::from_value(serde_json::to_value(request).unwrap()).unwrap();
    resumed.resumed_history = Some(
        rows.iter()
            .filter_map(|row| row.get("message"))
            .filter(|message| !message.is_null())
            .map(|message| serde_json::from_value(message.clone()).unwrap())
            .collect(),
    );
    resumed.instruction_context = Some(InstructionContext {
        announcement_history: rows
            .iter()
            .filter_map(|row| row.get("attachment").cloned())
            .collect(),
        ..Default::default()
    });
    let result = spawner.spawn(resumed, inheritance()).await.unwrap();
    let SubagentResult::Completed {
        agent_id: restored_id,
        ..
    } = result
    else {
        panic!("cold child must complete")
    };
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let text = prepared_child_text(&requests[2]);
    assert_eq!(text.matches("CURRENT MANAGED POLICY").count(), 1);
    assert!(text.contains("Instruction files were re-read when this session started; these differ from their earlier copies"));
    assert!(!text.contains("# instructions"));
    let saved =
        std::fs::read_to_string(directory.path().join(format!("agent-{restored_id}.jsonl")))
            .unwrap();
    assert!(!saved.contains("instruction_context"));
    assert!(saved
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .any(|row| row["attachment"]["type"] == "instructions"
            && row["attachment"]["changed"] == true));
}
