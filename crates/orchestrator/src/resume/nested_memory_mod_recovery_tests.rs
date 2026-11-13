//! Public production-Read -> nested-memory -> physical-request regression probe.
//! No private orchestrator state or restoration helper is used.
use crate::test_support::{
    content_block_start_text, content_block_start_tool_use, content_block_stop, input_json_delta,
    message_delta_stop, message_start, message_stop, mock_message_response, noop_hook_executor,
    text_delta, MockApiClient, MockOutputStream, MockStreamingApiClient, NoOpPermissionGate,
    StaticMemoryProvider,
};
use crate::{ConversationOrchestrator, OrchestratorConfig};
use lingxi_core::host::{FileSystem, OrchestratorHandle};
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection as Source, ConversationMessage, MessageId, SessionId,
};
use llm_runtime::{ContentBlock, HistoryEvent, HistoryResponse};
use platform_posix::fs::PosixFileSystem;
use serde_json::{json, Value};
use session::jsonl::{reader::JsonlReader, writer::JsonlWriter};
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

const MODEL: &str = "claude-opus-4-7";
const SECRET: &str = "NESTED_RAW_SOURCE_MUST_KEEP_MOD_PROJECTION";

fn replies(trigger: &std::path::Path) -> Vec<HistoryResponse> {
    (0..4)
        .map(|index| {
            let mut response = if index == 0 {
                mock_message_response(
                    vec![ContentBlock::ToolCall {
                        id: "call_read_nested".into(),
                        name: "Read".into(),
                        input: lingxi_llm_client::protocol::JsonPayload::parse(
                            &json!({"file_path":trigger}).to_string(),
                        )
                        .unwrap(),
                    }],
                    Some("tool_use"),
                )
            } else {
                mock_message_response(
                    vec![ContentBlock::Text {
                        text: "done".into(),
                        cache_control: None,
                        citations: None,
                    }],
                    Some("end_turn"),
                )
            };
            response.model = MODEL.into();
            response
        })
        .collect()
}

fn streams(responses: &[HistoryResponse]) -> Vec<Vec<HistoryEvent>> {
    responses
        .iter()
        .map(|response| {
            let mut events = vec![message_start(&response.id, MODEL)];
            match &response.content[0] {
                ContentBlock::ToolCall { id, name, input } => {
                    events.extend([
                        content_block_start_tool_use(0, id.clone().into(), name),
                        input_json_delta(0, input.as_json_source()),
                    ]);
                }
                ContentBlock::Text { text, .. } => {
                    events.extend([content_block_start_text(0), text_delta(0, text)])
                }
                _ => unreachable!(),
            }
            events.extend([
                content_block_stop(0),
                message_delta_stop(response.stop_reason.as_deref().unwrap()),
                message_stop(),
            ]);
            events
        })
        .collect()
}

async fn run(orch: &ConversationOrchestrator, streaming: bool, prompt: &str) {
    if streaming {
        orch.run_turn_streaming(prompt).await.unwrap();
    } else {
        orch.run_turn(prompt).await.unwrap();
    }
}
async fn captures(
    api: &MockApiClient,
    stream: &MockStreamingApiClient,
    streaming: bool,
) -> Vec<Vec<ConversationMessage>> {
    if streaming {
        stream
            .captured_calls()
            .await
            .into_iter()
            .map(|call| call.messages)
            .collect()
    } else {
        api.captured_msgs().await
    }
}
fn texts(messages: &[ConversationMessage], id: MessageId) -> Vec<String> {
    messages
        .iter()
        .filter(|row| row.id() == id)
        .map(ConversationMessage::text_content)
        .collect()
}

async fn one_case(streaming: bool, suppress: bool, recovery: u8, steering: bool) -> Value {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let cwd = root.join("repo");
    let home = root.join("home");
    let trigger = cwd.join("pkg/api/handler.rs");
    let nested = cwd.join("pkg/LINGXI.md");
    std::fs::create_dir_all(trigger.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(&trigger, "fn handler() {}\n").unwrap();
    std::fs::write(&nested, SECRET).unwrap();
    let module = root.join("nested-mod.js");
    let action = if suppress {
        "() => ({text:null})"
    } else {
        "($,e,next) => next({...e,text:'MOD: '+e.text})"
    };
    std::fs::write(&module, format!(
        "export function register(on) {{ on('prompt.attachment', {{type:'nested_memory'}}, {action}); }}"
    )).unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "nested-probe",
        &root,
        &module,
        tool_api::tool_trait::generated_builtin_result(&json!({})),
    )
    .await
    .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    let registry = Arc::new(tokio::sync::RwLock::new(registry));
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(root));
    let state = tool_api::read_file_state::new_read_file_state_map();
    let mut context = tool_api::test_support::ctx_for_file_tools(
        fs.clone(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![cwd.clone()],
    );
    context.read_file_state = state.clone();
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(tool_file::FileReadTool::new(context)));
    let tools = Arc::new(tools);
    let config = OrchestratorConfig {
        model: MODEL.into(),
        max_turns: 8,
        system_prompt_override: Some(Source::from("nested memory lifecycle")),
        ..Default::default()
    };
    let responses = replies(&trigger);
    let api = Arc::new(MockApiClient::new(responses.clone()));
    let stream = Arc::new(MockStreamingApiClient::with_turns(streams(&responses)));
    let session_id = SessionId::new();
    let transcript = session::jsonl::path::session_path(
        &home,
        &cwd.to_string_lossy(),
        &session_id.as_uuid().to_string(),
    );
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            config.clone(),
            api.clone(),
            stream.clone(),
            tools.clone(),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.clone(),
        )
        .with_read_state_map(state.clone())
        .with_nested_memory_roots(home.clone(), None)
        .with_mid_turn_input(Arc::new(AfterReadInput {
            state: state.clone(),
            trigger: trigger.clone(),
            delivered: std::sync::atomic::AtomicBool::new(!steering),
        }))
        .with_config_home(home.clone())
        .with_session_id(session_id)
        .with_jsonl_writer(Arc::new(JsonlWriter::new(transcript.clone(), fs.clone())))
        .with_hook_registry(registry.clone()),
    );
    orch.initialize_conversation_mode().await.unwrap();
    run(&orch, streaming, "read handler").await;
    let rows = JsonlReader::new(transcript.clone(), fs.clone())
        .read_all()
        .await
        .unwrap();
    let attachments: Vec<_> = rows
        .iter()
        .filter(|row| {
            row.message_type == "attachment"
                && row
                    .attachment_projection
                    .as_ref()
                    .and_then(|p| p.string_at("/type"))
                    == Some("nested_memory")
        })
        .collect();
    assert_eq!(
        attachments.len(),
        1,
        "production Read must discover one nested memory"
    );
    let id = MessageId::from_uuid(uuid::Uuid::parse_str(&attachments[0].uuid).unwrap());
    let original = texts(&orch.snapshot_history().await, id);
    let before = captures(&api, &stream, streaming).await;
    assert_eq!(before.len(), 2, "Read tool round plus end-turn reply");
    if steering {
        assert!(before[1]
            .iter()
            .any(|message| message.text_content().contains("live steering after Read")));
    }
    let fresh = texts(&before[1], id);
    let expected = if suppress {
        Vec::new()
    } else {
        vec![original[0].replace("Contents of ", "MOD: Contents of ")]
    };
    assert_eq!(
        fresh, expected,
        "fresh producer must demonstrate configured Mod"
    );
    if recovery == 1 {
        let replayed = crate::resume::replay_session_state(
            &home,
            &cwd.to_string_lossy(),
            session_id.as_uuid(),
            fs.clone(),
        )
        .await
        .unwrap();
        let runtime = replayed.handle_runtime_snapshot();
        OrchestratorHandle::resume_session(
            &*orch,
            session_id,
            replayed.state.history,
            replayed.last_message_uuid.map(|id| id.to_string()),
            None,
            runtime,
        )
        .await
        .unwrap();
    }
    let orch = if recovery == 2 {
        let mut cold = ConversationOrchestrator::with_resume(
            config,
            session_id.as_uuid(),
            home.clone(),
            cwd.to_string_lossy().into_owned(),
            fs.clone(),
            api.clone(),
            tools,
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd,
            Some(Arc::new(JsonlWriter::new(transcript.clone(), fs.clone()))),
        )
        .await
        .unwrap()
        .with_hook_registry(registry)
        .with_nested_memory_roots(home, None)
        .with_read_state_map(state);
        cold.streaming_api = stream.clone();
        ConversationOrchestrator::into_shared(cold)
    } else {
        orch
    };
    run(&orch, streaming, "follow-up without another Read").await;
    let after = captures(&api, &stream, streaming).await;
    let later = texts(&after[2], id);
    assert_eq!(
        later, expected,
        "nested memory must preserve its Mod projection across recovery"
    );
    assert_eq!(texts(&orch.snapshot_history().await, id), original);
    let persisted = JsonlReader::new(transcript, fs).read_all().await.unwrap();
    let copies = persisted
        .iter()
        .filter(|row| row.uuid == id.as_uuid().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        copies.len(),
        1,
        "raw UUID is never repersisted on request or resume"
    );
    assert_eq!(
        copies[0].attachment_projection.as_ref(),
        attachments[0].attachment_projection.as_ref()
    );
    let result = json!({"driver":if streaming {"streaming"} else {"batched"},
        "mod":if suppress {"suppress"} else {"rewrite"}, "recovery":recovery, "live_steering":steering,
        "fresh":fresh, "expected_later":expected, "actual_later":later,
        "raw_still_in_history":texts(&orch.snapshot_history().await, id) == original,
        "mod_still_registered":host.has_event("prompt.attachment"),
        "passed":later == expected});
    result
}

struct AfterReadInput {
    state: tool_api::read_file_state::ReadFileStateMap,
    trigger: std::path::PathBuf,
    delivered: std::sync::atomic::AtomicBool,
}
#[async_trait::async_trait]
impl crate::prompt::mid_turn_input::MidTurnInputSource for AfterReadInput {
    async fn take_mid_turn_input(&self) -> Option<Source> {
        (tool_api::read_file_state::get(&self.state, &self.trigger).is_some()
            && !self
                .delivered
                .swap(true, std::sync::atomic::Ordering::SeqCst))
        .then(|| Source::from("live steering after Read"))
    }
}

#[test]
fn real_nested_memory_mod_views_survive_followup_hot_cold_and_live_steering_both_drivers() {
    std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                for streaming in [false, true] {
                    for suppress in [false, true] {
                        for recovery in [0, 1, 2] {
                            for steering in [false, true] {
                                let result =
                                    one_case(streaming, suppress, recovery, steering).await;
                                assert_eq!(result["passed"], true, "{result:#}");
                            }
                        }
                    }
                }
            });
        })
        .unwrap()
        .join()
        .unwrap();
}
