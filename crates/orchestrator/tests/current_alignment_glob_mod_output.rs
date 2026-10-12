//! Replay Native 2.1.293 Glob output-parser failures through the current
//! production ModHost -> Orchestrator -> SDK ProviderApiAdapter -> OutputStream/session-JSONL path.
//!
//! The model response and filesystem are local fixed fixtures. The tested
//! result still comes from the production Glob tool and is rewritten by the
//! production Mods Bun worker before the Orchestrator invokes `parse_output`.
//! JSONL UUIDs/timestamps and the temporary cwd are checked as independent
//! fields; only fixed content sub-artifacts are compared with Native.

use async_trait::async_trait;
use lingxi_core::host::{FileSystem, OutputEvent};
use lingxi_core::types::{utf16_json::Utf16JsonProjection, SessionId};
use llm_runtime::model::{thinking::ThinkingConfig, user_agent::UserAgentEnv};
use llm_runtime::{ApiService, ClientConfig, ModelRuntime, SubscriberState, Transport};
use orchestrator::test_support::{
    noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{
    ConversationOrchestrator, ConversationOutcome, OrchestratorConfig, ProviderApiAdapter,
};
use platform_posix::fs::PosixFileSystem;
use session::jsonl::{reader::JsonlReader, writer::JsonlWriter};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::tempdir;
use tool_api::registry::ToolRegistry;

const MODEL: &str = "claude-sonnet-4-5";
const TOOL_USE_ID: &str = "toolu_native_glob_output_probe_293";
const SESSION_UUID: &str = "00000000-0000-4000-8000-000000002f01";
const OLD_TOKEN_REMINDER: &str =
    "<system-reminder>\n<total_tokens>15000000 tokens left</total_tokens>\n</system-reminder>";

struct FixedNativeGlobTransport {
    response_bodies: Mutex<VecDeque<Vec<u8>>>,
    request_bodies: Mutex<Vec<Vec<u8>>>,
}

#[async_trait]
impl Transport for FixedNativeGlobTransport {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        self.request_bodies
            .lock()
            .expect("request-body capture lock")
            .push(request.body.to_vec());
        let response_body = self
            .response_bodies
            .lock()
            .expect("fixed SSE queue lock")
            .pop_front()
            .expect("exactly two Native SSE responses are configured");
        Ok(lingxi_llm_client::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: response_body.into(),
        }
        .into())
    }
}

fn api_service(transport: Arc<FixedNativeGlobTransport>) -> Arc<ApiService> {
    let config: ClientConfig = serde_json::from_value(serde_json::json!({
        "providers": [{
            "provider_id": "anthropic_first_party",
            "profile_name": "direct",
            "base_url": "https://api.anthropic.com",
            "protocol": "anthropic_messages",
            "auth": "none",
            "credential": {"type": "none"},
            "models": [{
                "display_model": MODEL,
                "request_model": MODEL,
                "billing_model": MODEL,
                "capabilities": {
                    "streaming": true,
                    "tools": true,
                    "vision": false,
                    "documents": false,
                    "reasoning": false,
                    "structured_output": false
                }
            }]
        }]
    }))
    .expect("fixed first-party Anthropic route config");

    Arc::new(
        ApiService::new_with_routing(
            Arc::new(ModelRuntime::from_config(config).expect("model runtime")),
            transport,
            SubscriberState::default(),
            UserAgentEnv::default(),
            "current-alignment-glob-output",
            None,
            None,
            None,
            Default::default(),
            Some(0),
            None,
        )
        .with_thinking(ThinkingConfig::Disabled),
    )
}

struct NativeExpected {
    model_text_units: Vec<u16>,
    tool_use_result_units: Vec<u16>,
    error_units: Vec<u16>,
    model_tool_result_content: String,
    total_tokens_reminder: String,
    total_tokens_value: String,
    native_row: Utf16JsonProjection,
}

#[derive(Debug)]
struct RequestReminder {
    message_index: usize,
    block_index: usize,
    role: String,
    block_type: String,
    reminder_units: Vec<u16>,
}

#[derive(Debug)]
struct RequestToolResult {
    message_index: usize,
    block_index: usize,
    content_units: Vec<u16>,
    is_error: Option<bool>,
    tool_use_id: String,
}

fn total_tokens_value(reminder: &str) -> String {
    let prefix = "<system-reminder>\n<total_tokens>";
    let suffix = " tokens left</total_tokens>\n</system-reminder>";
    let body = reminder
        .strip_prefix(prefix)
        .and_then(|body| body.strip_suffix(suffix))
        .expect("fixed total-token reminder envelope");
    assert!(body == "Infinite" || body.parse::<u64>().is_ok());
    body.to_owned()
}

fn utf16_index_of(haystack: &[u16], needle: &[u16]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn total_tokens_reminder_units(text_units: &[u16]) -> Option<Vec<u16>> {
    let opening = "<system-reminder>\n<total_tokens>"
        .encode_utf16()
        .collect::<Vec<_>>();
    let closing = "</system-reminder>".encode_utf16().collect::<Vec<_>>();
    let start = utf16_index_of(text_units, &opening)?;
    let content_start = start + opening.len();
    let closing_start = utf16_index_of(&text_units[content_start..], &closing)? + content_start;
    let end = closing_start + closing.len();
    Some(text_units[start..end].to_vec())
}

fn request_reminders(request: &Utf16JsonProjection) -> Vec<RequestReminder> {
    let message_count = request
        .array_len("/messages")
        .expect("SDK request has messages array");
    let mut reminders = Vec::new();
    for message_index in 0..message_count {
        let message_pointer = format!("/messages/{message_index}");
        let role_pointer = format!("{message_pointer}/role");
        let role = request.string_at(&role_pointer).unwrap_or("").to_owned();
        let content_pointer = format!("{message_pointer}/content");
        let block_count = request
            .array_len(&content_pointer)
            .expect("SDK message content is an array");
        for block_index in 0..block_count {
            let block_pointer = format!("{content_pointer}/{block_index}");
            let Some(block_type) = request.string_at(&format!("{block_pointer}/type")) else {
                continue;
            };
            let text_pointer = match block_type {
                "text" => format!("{block_pointer}/text"),
                "tool_result" => format!("{block_pointer}/content"),
                _ => continue,
            };
            let Some(text_units) = request.string_units(&text_pointer) else {
                continue;
            };
            if let Some(reminder_units) = total_tokens_reminder_units(&text_units) {
                reminders.push(RequestReminder {
                    message_index,
                    block_index,
                    role: role.clone(),
                    block_type: block_type.to_owned(),
                    reminder_units,
                });
            }
        }
    }
    reminders
}

fn request_tool_use_location(
    request: &Utf16JsonProjection,
    provider_id: &str,
) -> Option<(usize, usize)> {
    let message_count = request.array_len("/messages")?;
    for message_index in 0..message_count {
        let message_pointer = format!("/messages/{message_index}");
        if request.string_at(&format!("{message_pointer}/role")) != Some("assistant") {
            continue;
        }
        let content_pointer = format!("{message_pointer}/content");
        let block_count = request.array_len(&content_pointer)?;
        for block_index in 0..block_count {
            let block_pointer = format!("{content_pointer}/{block_index}");
            if request.string_at(&format!("{block_pointer}/type")) == Some("tool_use")
                && request.string_at(&format!("{block_pointer}/id")) == Some(provider_id)
                && request.string_at(&format!("{block_pointer}/name")) == Some("Glob")
            {
                return Some((message_index, block_index));
            }
        }
    }
    None
}

fn request_tool_result(
    request: &Utf16JsonProjection,
    provider_id: &str,
) -> Option<RequestToolResult> {
    let message_count = request.array_len("/messages")?;
    for message_index in 0..message_count {
        let message_pointer = format!("/messages/{message_index}");
        if request.string_at(&format!("{message_pointer}/role")) != Some("user") {
            continue;
        }
        let content_pointer = format!("{message_pointer}/content");
        let block_count = request.array_len(&content_pointer)?;
        for block_index in 0..block_count {
            let block_pointer = format!("{content_pointer}/{block_index}");
            if request.string_at(&format!("{block_pointer}/type")) != Some("tool_result")
                || request.string_at(&format!("{block_pointer}/tool_use_id")) != Some(provider_id)
            {
                continue;
            }
            return Some(RequestToolResult {
                message_index,
                block_index,
                content_units: request.string_units(&format!("{block_pointer}/content"))?,
                is_error: request
                    .at_pointer(&format!("{block_pointer}/is_error"))
                    .and_then(|value| value.as_bool()),
                tool_use_id: request
                    .string_at(&format!("{block_pointer}/tool_use_id"))?
                    .to_owned(),
            });
        }
    }
    None
}

fn assert_native_response_usage(case: &str, response_index: usize) {
    let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join(
        "../../docs/parity/current-alignment/fixtures/tool-output-parser/native-baseline/2.1.293",
    );
    let raw = std::fs::read_to_string(
        fixture_root
            .join(case)
            .join(format!("response-{response_index:02}.raw.sse")),
    )
    .expect("captured Native response SSE");
    let mut input_tokens = None;
    let mut output_tokens = None;
    for line in raw.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        let event: serde_json::Value = serde_json::from_str(data).expect("Native SSE data JSON");
        let event_type = event.get("type").and_then(serde_json::Value::as_str);
        let usage = match event_type {
            Some("message_start") => event.pointer("/message/usage"),
            Some("message_delta") => event.get("usage"),
            _ => None,
        };
        let Some(usage) = usage else {
            continue;
        };
        if event_type == Some("message_start") {
            input_tokens = usage
                .get("input_tokens")
                .and_then(serde_json::Value::as_u64);
        } else if event_type == Some("message_delta") {
            output_tokens = usage
                .get("output_tokens")
                .and_then(serde_json::Value::as_u64);
        }
        for field in ["cache_read_input_tokens", "cache_creation_input_tokens"] {
            assert_eq!(
                usage
                    .get(field)
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
                0,
                "the Native SSE fixture has no cached-token counts: {case}/{response_index}"
            );
        }
    }
    let input_tokens = input_tokens.expect("Native message_start input_tokens");
    let output_tokens = output_tokens.expect("Native message_delta output_tokens");
    let expected_usage = match response_index {
        1 => (5, 1),
        2 => (5, 2),
        other => panic!("no fixed Native usage assertion for response {other}"),
    };
    assert_eq!(
        (input_tokens, output_tokens),
        expected_usage,
        "fixed Native response usage remains part of this fixture: {case}/{response_index}"
    );
}

fn native_expected(case: &str) -> NativeExpected {
    let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join(
        "../../docs/parity/current-alignment/fixtures/tool-output-parser/native-baseline/2.1.293",
    );
    let raw_row = std::fs::read_to_string(
        fixture_root
            .join(case)
            .join("tool-use-result-session-rows.raw.jsonl"),
    )
    .expect("captured Native tool-result JSONL row");
    let raw_line = raw_row
        .lines()
        .next()
        .expect("one captured tool-result row");
    let native_row = Utf16JsonProjection::parse(raw_line).expect("raw Native JSONL row");
    let model_text_units = native_row
        .string_units("/message/content/0/content")
        .expect("Native model-facing tool result");
    let tool_use_result_units = native_row
        .string_units("/toolUseResult")
        .expect("Native persisted toolUseResult");
    let model_tool_result_content = std::fs::read_to_string(
        fixture_root
            .join(case)
            .join("model-tool-result-content.raw.txt"),
    )
    .expect("captured Native model-facing content with its dynamic reminder");
    let native_reminder = model_tool_result_content
        .rsplit_once("\n\n")
        .map(|(_, reminder)| reminder.to_owned())
        .expect("Native token reminder follows the fixed error body");
    let total_tokens_value = total_tokens_value(&native_reminder);
    let direct_tool_result_path = fixture_root.join(case).join("tool-use-result.raw.json");
    if direct_tool_result_path.exists() {
        let direct_tool_result_raw = std::fs::read_to_string(direct_tool_result_path)
            .expect("captured direct Native toolUseResult slice");
        let direct_tool_result = Utf16JsonProjection::parse(&direct_tool_result_raw)
            .expect("raw Native toolUseResult JSON string");
        assert_eq!(
            direct_tool_result.string_units(""),
            Some(tool_use_result_units.clone()),
            "separately captured raw toolUseResult agrees with the Native JSONL field: {case}"
        );
    }

    // The OutputEvent result payload stores the issue message without either
    // Native presentation wrapper. This is a field-level assertion only; the
    // complete Native model_text and toolUseResult strings are compared below.
    let model_prefix = "<tool_use_error>".encode_utf16().collect::<Vec<_>>();
    let model_suffix = "</tool_use_error>".encode_utf16().collect::<Vec<_>>();
    assert!(model_text_units.starts_with(&model_prefix));
    assert!(model_text_units.ends_with(&model_suffix));
    let error_units =
        model_text_units[model_prefix.len()..model_text_units.len() - model_suffix.len()].to_vec();

    NativeExpected {
        model_text_units,
        tool_use_result_units,
        error_units,
        model_tool_result_content,
        total_tokens_reminder: native_reminder,
        total_tokens_value,
        native_row,
    }
}

fn fixed_session_id() -> SessionId {
    SessionId::from_uuid(uuid::Uuid::parse_str(SESSION_UUID).expect("fixed session UUID"))
}

fn write_glob_mod(module_dir: &Path, duration_expression: &str) -> PathBuf {
    std::fs::create_dir_all(module_dir).expect("create isolated Mod directory");
    let module = module_dir.join("register.js");
    let source = format!(
        r#"
        export function register(on) {{
          on('tool.call', {{ tool: 'Glob' }}, async ($, event, next) => {{
            const nativeResult = await next(event);
            if (!nativeResult?.result || nativeResult.result.filenames?.[0] !== 'probe.fixture') {{
              throw new Error('fixed production Glob next result was not observed');
            }}
            const result = {{
              durationMs: {duration_expression},
              numFiles: 1,
              filenames: ['hook-return.fixture'],
              truncated: false,
              totalMatches: 1,
              countIsComplete: true,
            }};
            return {{
              ...nativeResult,
              result,
              text: result.filenames.join('\n'),
            }};
          }});
        }}
        "#
    );
    std::fs::write(&module, source).expect("write fixed Glob Mod middleware");
    module
}

async fn replay_case(case: &str, duration_expression: &str) {
    let expected = native_expected(case);
    let temp = tempdir().expect("isolated fixture root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("create isolated Glob workspace");
    std::fs::write(
        workspace.join("probe.fixture"),
        b"fixed read-only Glob input",
    )
    .expect("create fixed Glob match");

    let module = write_glob_mod(&temp.path().join("plugin"), duration_expression);
    let host = hooks::mods::ModHost::start(None)
        .await
        .expect("start current production Mod worker");
    host.load(
        "current-alignment-glob-output",
        module.parent().expect("Mod directory"),
        &module,
        Utf16JsonProjection::parse("{}").expect("empty Mod options JSON"),
    )
    .await
    .expect("load fixed public tool.call middleware");

    let file_system: Arc<dyn FileSystem> =
        Arc::new(PosixFileSystem::new(temp.path().to_path_buf()));
    let session_path = temp.path().join("session.jsonl");
    let writer = Arc::new(JsonlWriter::new(session_path.clone(), file_system.clone()));
    let tool_context = tool_api::test_support::ctx_for_file_tools(
        file_system.clone(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![workspace.clone()],
    );
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(tool_file::GlobTool::new(tool_context)));

    let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join(
        "../../docs/parity/current-alignment/fixtures/tool-output-parser/native-baseline/2.1.293",
    );
    assert_native_response_usage(case, 1);
    assert_native_response_usage(case, 2);
    let response_bodies = (1..=2)
        .map(|response_index| {
            std::fs::read(
                fixture_root
                    .join(case)
                    .join(format!("response-{response_index:02}.raw.sse")),
            )
            .expect("captured Native SSE bytes")
        })
        .collect::<VecDeque<_>>();
    let transport = Arc::new(FixedNativeGlobTransport {
        response_bodies: Mutex::new(response_bodies),
        request_bodies: Mutex::new(Vec::new()),
    });
    let adapter = Arc::new(ProviderApiAdapter::new(api_service(transport.clone())));
    let output = Arc::new(MockOutputStream::new());
    let mut hook_registry = hooks::HookRegistry::new();
    hook_registry.set_mod_host(host);
    let orchestrator = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                model: MODEL.to_owned(),
                max_turns: 2,
                ..OrchestratorConfig::default()
            },
            adapter.clone(),
            adapter,
            Arc::new(tools),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            workspace.clone(),
        )
        .with_session_id(fixed_session_id())
        .with_jsonl_writer(writer)
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(hook_registry))),
    );

    let outcome = orchestrator
        .run_turn_streaming("Find the file matching *.fixture and report its path.")
        .await
        .expect("fixed two-response turn");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    // Inspect the exact request bodies received by the local Transport. They
    // are the SDK's outgoing bytes; parsing below only selects source strings
    // and never normalizes or reserializes the candidate request.
    let request_bodies = transport
        .request_bodies
        .lock()
        .expect("captured SDK request bodies")
        .clone();
    assert_eq!(request_bodies.len(), 2, "two Native-shaped SDK requests");
    assert!(
        transport
            .response_bodies
            .lock()
            .expect("fixed SSE queue")
            .is_empty(),
        "both fixed Native SSE responses were consumed without retry"
    );

    let parse_request = |raw: &[u8]| {
        Utf16JsonProjection::parse(
            std::str::from_utf8(raw).expect("SDK request body is UTF-8 JSON"),
        )
        .expect("parse captured raw SDK body without reserializing it")
    };
    let first_request = parse_request(&request_bodies[0]);
    let second_request = parse_request(&request_bodies[1]);
    for (index, request) in [&first_request, &second_request].into_iter().enumerate() {
        assert_eq!(request.string_at("/model"), Some(MODEL));
        assert_eq!(
            request
                .at_pointer("/stream")
                .and_then(|value| value.as_bool()),
            Some(true),
            "request {index} uses the actual streaming SDK path"
        );
    }

    // Native request-01 contains the old user-context reminder. Request-02
    // retains that same fragment, then places the new reminder inside the
    // tool_result content. This distinguishes retained context from a
    // duplicate post-tool reminder.
    let first_reminders = request_reminders(&first_request);
    assert_eq!(first_reminders.len(), 1, "one reminder in request-01");
    assert_eq!(first_reminders[0].role, "user");
    assert_eq!(first_reminders[0].block_type, "text");
    assert_eq!(
        first_reminders[0].reminder_units,
        OLD_TOKEN_REMINDER.encode_utf16().collect::<Vec<_>>(),
        "request-01 carries Native's fixed old reminder"
    );

    let candidate_reminders = request_reminders(&second_request);
    assert_eq!(
        candidate_reminders.len(),
        2,
        "request-02 has the retained context and current tool-result reminders: {candidate_reminders:?}"
    );
    assert_eq!(candidate_reminders[0].role, "user");
    assert_eq!(candidate_reminders[0].block_type, "text");
    assert_eq!(
        candidate_reminders[0].reminder_units,
        OLD_TOKEN_REMINDER.encode_utf16().collect::<Vec<_>>(),
        "request-02 retains the prior user-context reminder"
    );
    assert_eq!(candidate_reminders[1].role, "user");
    assert_eq!(candidate_reminders[1].block_type, "tool_result");
    assert_eq!(
        candidate_reminders[1].reminder_units,
        expected
            .total_tokens_reminder
            .encode_utf16()
            .collect::<Vec<_>>(),
        "the current Native reminder is at the end of ToolResult wire content"
    );
    let candidate_total_tokens_reminder =
        String::from_utf16(&candidate_reminders[1].reminder_units)
            .expect("fixed reminder contains scalar text");
    let candidate_total_tokens_value = total_tokens_value(&candidate_total_tokens_reminder);
    assert_eq!(
        candidate_total_tokens_value, expected.total_tokens_value,
        "the current reminder uses Native's fixed response-02 usage 5/2"
    );

    let (tool_use_message_index, _) = request_tool_use_location(&second_request, TOOL_USE_ID)
        .expect("request-02 contains the fixed Glob tool_use");
    let tool_result = request_tool_result(&second_request, TOOL_USE_ID)
        .expect("request-02 contains the paired Glob tool_result");
    assert!(
        candidate_reminders[0].message_index < tool_use_message_index
            && tool_use_message_index < tool_result.message_index
            && tool_result.message_index == candidate_reminders[1].message_index
            && tool_result.block_index == candidate_reminders[1].block_index,
        "Native order is prior context, assistant tool_use, then tool_result with its current reminder: {candidate_reminders:?}, tool_result={tool_result:?}"
    );
    let error_flag_artifact = if tool_result.is_error != Some(true) {
        let directory = std::env::temp_dir().join(format!(
            "current-alignment-glob-mod-output-{}-{}",
            case,
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&directory).expect("create candidate wire diagnostic directory");
        let request_path = directory.join("candidate-request-02.raw.json");
        std::fs::write(&request_path, &request_bodies[1])
            .expect("save raw candidate SDK request containing the mismatched ToolResult");
        let session_copy = directory.join("candidate-session.jsonl");
        if session_path.is_file() {
            std::fs::copy(&session_path, &session_copy)
                .expect("save candidate session JSONL for the same ToolResult");
        }
        Some((request_path, session_copy))
    } else {
        None
    };
    assert_eq!(
        tool_result.is_error,
        Some(true),
        "Native parser failure must be marked on the same provider ToolResult; candidate raw capture: {error_flag_artifact:?}"
    );
    assert_eq!(tool_result.tool_use_id, TOOL_USE_ID);
    assert!(
        tool_result
            .content_units
            .starts_with(&"<tool_use_error>".encode_utf16().collect::<Vec<_>>()),
        "typed Glob output validation uses Native's model-facing error wrapper"
    );
    assert_eq!(
        tool_result.content_units,
        expected
            .model_tool_result_content
            .encode_utf16()
            .collect::<Vec<_>>(),
        "the complete Native tool-result model content, including reminder, matches the actual SDK body"
    );
    let events = output.snapshot().await;
    let tool_events = events
        .iter()
        .filter_map(|event| match event {
            OutputEvent::ToolResult {
                id,
                tool,
                model_text,
                result,
            } if id.as_str() == TOOL_USE_ID && tool == "Glob" => Some((event, model_text, result)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        tool_events.len(),
        1,
        "one error result should reach OutputStream"
    );
    let (event, model_text, result) = tool_events[0];
    let actual_model_units = model_text.string_units("").expect("OutputEvent model text");
    assert_eq!(actual_model_units, expected.model_text_units, "{case}");
    let actual_model_utf8 = String::from_utf16(&actual_model_units)
        .expect("Native parser error has scalar text")
        .into_bytes();
    let expected_model_utf8 = String::from_utf16(&expected.model_text_units)
        .expect("Native parser error has scalar text")
        .into_bytes();
    assert_eq!(actual_model_utf8, expected_model_utf8, "{case}");
    let actual_error_units = result
        .string_units("/error")
        .expect("structured OutputEvent error text");
    assert_eq!(actual_error_units, expected.error_units, "{case}");
    assert_eq!(
        result.object_entries().map(|entries| entries.len()),
        Some(1),
        "the replaced Glob result publishes only its bare host error payload; the model-only wrapper and original filenames do not leak into OutputEvent: {case}"
    );

    let serialized_event = serde_json::to_string(event).expect("serialize OutputStream event");
    let serialized_projection =
        Utf16JsonProjection::parse(&serialized_event).expect("serialized OutputEvent JSON");
    assert_eq!(
        serialized_projection.string_units("/ToolResult/model_text"),
        Some(expected.model_text_units.clone()),
        "OutputStream event serialization preserves the fixed Native error body: {case}"
    );
    assert_eq!(
        serialized_projection.string_units("/ToolResult/result/error"),
        Some(expected.error_units.clone()),
        "serialized OutputStream error payload preserves the Native issue text: {case}"
    );

    let raw_session = std::fs::read_to_string(&session_path).expect("read real session JSONL");
    let candidate_row_raw = raw_session
        .lines()
        .find(|line| {
            Utf16JsonProjection::parse(line).is_ok_and(|row| {
                row.string_at("/message/content/0/type") == Some("tool_result")
                    && row.string_at("/message/content/0/tool_use_id") == Some(TOOL_USE_ID)
            })
        })
        .expect("persisted row with fixed provider tool-use id");
    let candidate_row = Utf16JsonProjection::parse(candidate_row_raw)
        .expect("candidate raw JSONL row preserves source strings");
    assert_eq!(
        candidate_row.string_units("/message/content/0/content"),
        Some(expected.model_text_units.clone()),
        "persisted tool result content matches the complete fixed Native sub-artifact: {case}"
    );
    assert_eq!(
        candidate_row.string_units("/toolUseResult"),
        Some(expected.tool_use_result_units.clone()),
        "persisted toolUseResult matches the complete fixed Native sub-artifact: {case}"
    );
    assert_eq!(
        candidate_row.string_at("/message/content/0/tool_use_id"),
        Some(TOOL_USE_ID),
        "fixed tool-use id correlates across model, OutputStream and JSONL: {case}"
    );
    assert_eq!(
        expected
            .native_row
            .string_at("/message/content/0/tool_use_id"),
        Some(TOOL_USE_ID),
        "fixed Native tool-use id is part of the oracle"
    );
    assert_eq!(candidate_row.string_at("/sessionId"), Some(SESSION_UUID));
    assert_eq!(
        candidate_row.string_at("/cwd"),
        Some(workspace.to_str().unwrap())
    );

    // UUID and timestamp values are generated at runtime. Keep their native
    // contracts explicit without stripping them from a whole-row comparison.
    let typed_rows = JsonlReader::new(session_path, file_system)
        .read_all()
        .await
        .expect("typed read of persisted JSONL");
    let typed_tool_row = typed_rows
        .iter()
        .find(|row| {
            row.message_type == "user"
                && row.message.string_at("/content/0/type") == Some("tool_result")
                && row.message.string_at("/content/0/tool_use_id") == Some(TOOL_USE_ID)
        })
        .expect("typed tool-result row");
    assert!(session::jsonl::uuid::validate_uuid(&typed_tool_row.uuid));
    assert!(
        chrono::DateTime::parse_from_rfc3339(&typed_tool_row.timestamp).is_ok(),
        "generated timestamp retains RFC3339 encoding: {case}"
    );
    assert!(typed_tool_row.timestamp.ends_with('Z'));
    assert!(typed_tool_row
        .parent_uuid
        .as_deref()
        .is_some_and(session::jsonl::uuid::validate_uuid));
    assert_eq!(typed_tool_row.session_id, SESSION_UUID);
    assert_eq!(typed_tool_row.cwd, workspace.to_string_lossy());
    assert_eq!(
        typed_tool_row.message.string_at("/content/0/tool_use_id"),
        Some(TOOL_USE_ID)
    );

    // The captured Native row itself has dynamic UUID/timestamp/cwd values;
    // validate the reference fields without pretending its whole JSONL line is
    // byte-comparable to the current candidate row.
    assert!(
        session::jsonl::uuid::validate_uuid(
            expected
                .native_row
                .string_at("/uuid")
                .expect("Native row UUID")
        ),
        "Native tool-result UUID format is independently checked"
    );
    assert!(chrono::DateTime::parse_from_rfc3339(
        expected
            .native_row
            .string_at("/timestamp")
            .expect("Native row timestamp")
    )
    .is_ok());

    println!(
        "CURRENT_ALIGNMENT_GLOB_MOD_OUTPUT={}",
        serde_json::json!({
            "case": case,
            "fixed_tool_use_id": TOOL_USE_ID,
            "native_total_tokens_value": expected.total_tokens_value,
            "candidate_total_tokens_value": candidate_total_tokens_value,
            "candidate_jsonl_uuid": typed_tool_row.uuid,
            "candidate_jsonl_timestamp": typed_tool_row.timestamp,
        })
    );
}

#[tokio::test]
async fn native_293_glob_output_errors_reach_modhost_orchestrator_output_and_jsonl() {
    replay_case("glob-duration-infinity", "Infinity").await;
    replay_case("glob-duration-wrong-type", "'wrong'").await;
}
