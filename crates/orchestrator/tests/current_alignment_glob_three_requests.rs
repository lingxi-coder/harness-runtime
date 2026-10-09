//! Replay Native 2.1.293's two-Glob-call sequence through the current production
//! ModHost -> Orchestrator -> SDK -> OutputStream/session-JSONL path.
//!
//! The test compares selected source slices from the actual SDK request body
//! and transcript. Native-generated row IDs/timestamps remain separate format
//! constraints; this is not a whole-request or whole-row byte comparison.

use async_trait::async_trait;
use lingxi_core::host::{FileSystem, OutputEvent};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::SessionId;
use llm_runtime::model::{thinking::ThinkingConfig, user_agent::UserAgentEnv};
use llm_runtime::{ApiService, ClientConfig, ModelRuntime, SubscriberState, Transport};
use orchestrator::test_support::{
    noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{
    ConversationOrchestrator, ConversationOutcome, OrchestratorConfig, ProviderApiAdapter,
};
use platform_posix::fs::PosixFileSystem;
use session::jsonl::writer::JsonlWriter;
use std::collections::VecDeque;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::tempdir;
use tool_api::registry::ToolRegistry;

const MODEL: &str = "claude-sonnet-4-5";
const SESSION_UUID: &str = "00000000-0000-4000-8000-000000002f11";
const TOOL_USE_IDS: [&str; 2] = [
    "toolu_native_glob_three_request_01",
    "toolu_native_glob_three_request_02",
];
const PROMPT: &str = "Find the file matching *.fixture twice and report its path.";
const INITIAL_REMINDER: &str =
    "<system-reminder>\n<total_tokens>15000000 tokens left</total_tokens>\n</system-reminder>";
const FIRST_TOOL_REMINDER: &str =
    "<system-reminder>\n<total_tokens>14999994 tokens left</total_tokens>\n</system-reminder>";
const SECOND_TOOL_REMINDER: &str =
    "<system-reminder>\n<total_tokens>14999991 tokens left</total_tokens>\n</system-reminder>";

struct FixedNativeThreeRequestTransport {
    response_bodies: Mutex<VecDeque<Vec<u8>>>,
    request_bodies: Mutex<Vec<Vec<u8>>>,
}

#[async_trait]
impl Transport for FixedNativeThreeRequestTransport {
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
            .expect("exactly three Native SSE responses are configured");
        Ok(lingxi_llm_client::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: response_body.into(),
        }
        .into())
    }
}

fn api_service(transport: Arc<FixedNativeThreeRequestTransport>) -> Arc<ApiService> {
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
            "current-alignment-glob-three-requests",
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

fn fixed_session_id() -> SessionId {
    SessionId::from_uuid(uuid::Uuid::parse_str(SESSION_UUID).expect("fixed session UUID"))
}

fn fixture_case() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(
        "../../docs/parity/current-alignment/fixtures/tool-output-parser/native-baseline/2.1.293/glob-three-requests",
    )
}

fn write_glob_mod(module_dir: &Path) -> PathBuf {
    std::fs::create_dir_all(module_dir).expect("create isolated Mod directory");
    let module = module_dir.join("register.js");
    std::fs::write(
        &module,
        r#"
        export function register(on) {
          on('tool.call', { tool: 'Glob' }, async ($, event, next) => {
            const nativeResult = await next(event);
            $.ui.log(`F_GLOB_NEXT=${JSON.stringify({
              keys: Object.keys(nativeResult ?? {}),
              resultType: typeof nativeResult?.result,
              resultKeys: nativeResult?.result && typeof nativeResult.result === 'object'
                ? Object.keys(nativeResult.result)
                : null,
              filenames: nativeResult?.result?.filenames,
              text: nativeResult?.text,
              isError: nativeResult?.isError,
            })}`);
            if (!nativeResult?.result || nativeResult.result.filenames?.[0] !== 'probe.fixture') {
              throw new Error('fixed production Glob next result was not observed');
            }
            const result = {
              durationMs: 1,
              numFiles: 1,
              filenames: ['hook-return.fixture'],
              truncated: false,
              totalMatches: 1,
              countIsComplete: true,
            };
            return {
              ...nativeResult,
              result,
              text: result.filenames.join('\n'),
            };
          });
        }
        "#,
    )
    .expect("write fixed Glob Mod middleware");
    module
}

#[derive(Clone, Copy)]
enum JsonPathSegment<'a> {
    Key(&'a str),
    Index(usize),
}

fn skip_json_whitespace(raw: &[u8], mut position: usize) -> usize {
    while raw
        .get(position)
        .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
    {
        position += 1;
    }
    position
}

fn json_string_end(raw: &[u8], start: usize) -> usize {
    assert_eq!(raw.get(start), Some(&b'"'), "JSON string starts with quote");
    let mut position = start + 1;
    while position < raw.len() {
        match raw[position] {
            b'\\' => position += 2,
            b'"' => return position + 1,
            _ => position += 1,
        }
    }
    panic!("unterminated JSON string")
}

fn json_value_end(raw: &[u8], start: usize) -> usize {
    let start = skip_json_whitespace(raw, start);
    match raw.get(start).copied().expect("JSON value start") {
        b'"' => json_string_end(raw, start),
        b'{' => {
            let mut position = skip_json_whitespace(raw, start + 1);
            if raw.get(position) == Some(&b'}') {
                return position + 1;
            }
            loop {
                position = json_string_end(raw, skip_json_whitespace(raw, position));
                position = skip_json_whitespace(raw, position);
                assert_eq!(raw.get(position), Some(&b':'), "JSON object colon");
                position = json_value_end(raw, position + 1);
                position = skip_json_whitespace(raw, position);
                match raw.get(position) {
                    Some(b',') => position += 1,
                    Some(b'}') => return position + 1,
                    other => panic!("invalid JSON object separator: {other:?}"),
                }
            }
        }
        b'[' => {
            let mut position = skip_json_whitespace(raw, start + 1);
            if raw.get(position) == Some(&b']') {
                return position + 1;
            }
            loop {
                position = json_value_end(raw, position);
                position = skip_json_whitespace(raw, position);
                match raw.get(position) {
                    Some(b',') => position += 1,
                    Some(b']') => return position + 1,
                    other => panic!("invalid JSON array separator: {other:?}"),
                }
            }
        }
        _ => {
            let mut position = start;
            while raw.get(position).is_some_and(|byte| {
                !matches!(byte, b',' | b']' | b'}' | b' ' | b'\t' | b'\r' | b'\n')
            }) {
                position += 1;
            }
            position
        }
    }
}

fn json_object_member_range(raw: &[u8], object_start: usize, wanted: &str) -> Range<usize> {
    assert_eq!(raw.get(object_start), Some(&b'{'), "JSON object start");
    let mut position = skip_json_whitespace(raw, object_start + 1);
    loop {
        assert_ne!(raw.get(position), Some(&b'}'), "JSON object member exists");
        let key_start = position;
        let key_end = json_string_end(raw, key_start);
        let key: String = serde_json::from_slice(&raw[key_start..key_end]).expect("JSON key");
        position = skip_json_whitespace(raw, key_end);
        assert_eq!(raw.get(position), Some(&b':'), "JSON object member colon");
        let value_start = skip_json_whitespace(raw, position + 1);
        let value_end = json_value_end(raw, value_start);
        if key == wanted {
            return value_start..value_end;
        }
        position = skip_json_whitespace(raw, value_end);
        match raw.get(position) {
            Some(b',') => position = skip_json_whitespace(raw, position + 1),
            Some(b'}') => panic!("JSON object has no member {wanted:?}"),
            other => panic!("invalid JSON object separator: {other:?}"),
        }
    }
}

fn json_array_element_range(raw: &[u8], array_start: usize, wanted: usize) -> Range<usize> {
    assert_eq!(raw.get(array_start), Some(&b'['), "JSON array start");
    let mut position = skip_json_whitespace(raw, array_start + 1);
    for index in 0..=wanted {
        let value_start = position;
        let value_end = json_value_end(raw, value_start);
        if index == wanted {
            return value_start..value_end;
        }
        position = skip_json_whitespace(raw, value_end);
        assert_eq!(raw.get(position), Some(&b','), "JSON array element exists");
        position = skip_json_whitespace(raw, position + 1);
    }
    unreachable!()
}

fn json_source_slice<'a>(raw: &'a [u8], path: &[JsonPathSegment<'_>]) -> &'a [u8] {
    let mut range = 0..json_value_end(raw, 0);
    for segment in path {
        range = match segment {
            JsonPathSegment::Key(key) => json_object_member_range(raw, range.start, key),
            JsonPathSegment::Index(index) => json_array_element_range(raw, range.start, *index),
        };
    }
    &raw[range]
}

fn projection(raw: &[u8]) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(std::str::from_utf8(raw).expect("fixture JSON is UTF-8"))
        .expect("parse raw JSON without reserializing")
}

fn assert_fixed_response_usage(case: &Path) {
    let expected = [(5, 1, "tool_use"), (7, 2, "tool_use"), (11, 3, "end_turn")];
    for (index, (expected_input, expected_output, expected_stop)) in
        expected.into_iter().enumerate()
    {
        let raw = std::fs::read(case.join(format!("response-{:02}.raw.sse", index + 1)))
            .expect("captured Native SSE bytes");
        let text = std::str::from_utf8(&raw).expect("Native SSE is UTF-8");
        let mut input_tokens = None;
        let mut output_tokens = None;
        let mut stop_reason: Option<String> = None;
        for line in text.lines() {
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let event: serde_json::Value = serde_json::from_str(data).expect("SSE event JSON");
            match event.get("type").and_then(serde_json::Value::as_str) {
                Some("message_start") => {
                    input_tokens = event
                        .pointer("/message/usage/input_tokens")
                        .and_then(serde_json::Value::as_u64);
                }
                Some("message_delta") => {
                    output_tokens = event
                        .pointer("/usage/output_tokens")
                        .and_then(serde_json::Value::as_u64);
                    stop_reason = event
                        .pointer("/delta/stop_reason")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                }
                _ => {}
            }
        }
        assert_eq!(input_tokens, Some(expected_input));
        assert_eq!(output_tokens, Some(expected_output));
        assert_eq!(stop_reason.as_deref(), Some(expected_stop));
    }
}

fn native_reminder_rows(case: &Path) -> Vec<Vec<u8>> {
    std::fs::read(case.join("token-reminder-attachments.raw.jsonl"))
        .expect("captured Native total-token reminder attachment rows")
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn attachment_keys(row: &Utf16JsonProjection) -> Vec<String> {
    row.at_pointer("/attachment")
        .expect("typed attachment object")
        .object_entries()
        .expect("attachment object entries")
        .into_iter()
        .map(|entry| entry.display_key.to_owned())
        .collect()
}

fn reminder_occurrences(request: &Utf16JsonProjection) -> Vec<(usize, usize, String, String)> {
    let mut occurrences = Vec::new();
    let message_count = request
        .array_len("/messages")
        .expect("request messages array");
    for message_index in 0..message_count {
        let content_pointer = format!("/messages/{message_index}/content");
        let block_count = request
            .array_len(&content_pointer)
            .expect("message content array");
        for block_index in 0..block_count {
            let block_pointer = format!("{content_pointer}/{block_index}");
            let Some(block_type) = request.string_at(&format!("{block_pointer}/type")) else {
                continue;
            };
            let (field, text_pointer) = match block_type {
                "text" => ("text", format!("{block_pointer}/text")),
                "tool_result" => ("content", format!("{block_pointer}/content")),
                _ => continue,
            };
            let Some(text_units) = request.string_units(&text_pointer) else {
                continue;
            };
            let text = String::from_utf16(&text_units).expect("fixed reminder text is scalar");
            let mut cursor = 0;
            while let Some(relative_start) = text[cursor..].find("<total_tokens>") {
                let value_start = cursor + relative_start + "<total_tokens>".len();
                let Some(relative_end) = text[value_start..].find(" tokens left</total_tokens>")
                else {
                    break;
                };
                let value_end = value_start + relative_end;
                occurrences.push((
                    message_index,
                    block_index,
                    field.to_owned(),
                    text[value_start..value_end].to_owned(),
                ));
                cursor = value_end + " tokens left</total_tokens>".len();
            }
        }
    }
    occurrences
}

#[tokio::test]
async fn native_three_request_glob_reminders_and_tool_results_reach_real_sdk_and_jsonl() {
    let case = fixture_case();
    assert_fixed_response_usage(&case);
    let response_bodies = (1..=3)
        .map(|response_index| {
            std::fs::read(case.join(format!("response-{response_index:02}.raw.sse")))
                .expect("captured Native SSE bytes")
        })
        .collect::<VecDeque<_>>();
    let transport = Arc::new(FixedNativeThreeRequestTransport {
        response_bodies: Mutex::new(response_bodies),
        request_bodies: Mutex::new(Vec::new()),
    });

    let temp = tempdir().expect("isolated fixture root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("create isolated Glob workspace");
    std::fs::write(
        workspace.join("probe.fixture"),
        b"fixed read-only Glob input",
    )
    .expect("create fixed Glob match");
    let module = write_glob_mod(&temp.path().join("plugin"));
    let host = hooks::mods::ModHost::start(None)
        .await
        .expect("start current production Mod worker");
    host.load(
        "current-alignment-glob-three-requests",
        module.parent().expect("Mod directory"),
        &module,
        Utf16JsonProjection::parse("{}").expect("empty Mod options"),
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

    let adapter = Arc::new(ProviderApiAdapter::new(api_service(transport.clone())));
    let output = Arc::new(MockOutputStream::new());
    let mut hook_registry = hooks::HookRegistry::new();
    hook_registry.set_mod_host(host);
    let orchestrator = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                model: MODEL.to_owned(),
                max_turns: 3,
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
        .run_turn_streaming(PROMPT)
        .await
        .expect("fixed three-response turn");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));
    let mod_log_trace = output
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event {
            OutputEvent::ModLog { plugin, text } => {
                Some((plugin, text.string_at("").unwrap_or_default().to_owned()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    eprintln!("CURRENT_ALIGNMENT_GLOB_NEXT_TRACE={mod_log_trace:?}");
    let request_bodies = transport
        .request_bodies
        .lock()
        .expect("captured SDK request bodies")
        .clone();
    assert_eq!(request_bodies.len(), 3, "two tools then one final answer");
    assert!(
        transport
            .response_bodies
            .lock()
            .expect("fixed SSE queue")
            .is_empty(),
        "all fixed Native responses were consumed without retry"
    );

    let native_request03 = std::fs::read(case.join("request-03.raw.json"))
        .expect("captured original Native request-03 bytes");
    let candidate_request03 = request_bodies.get(2).expect("captured SDK request-03");
    let native_request_projection = projection(&native_request03);
    let candidate_request_projection = projection(candidate_request03);
    assert_eq!(
        candidate_request_projection.string_at("/model"),
        Some(MODEL)
    );
    assert_eq!(
        candidate_request_projection
            .at_pointer("/stream")
            .and_then(|value| value.as_bool()),
        Some(true)
    );

    // Compare the exact source slices in the real SDK request body. This keeps
    // JSON escaping, field order, content, references, and cache metadata visible.
    let initial_reminder_path = [
        JsonPathSegment::Key("messages"),
        JsonPathSegment::Index(0),
        JsonPathSegment::Key("content"),
        JsonPathSegment::Index(2),
    ];
    assert_eq!(
        json_source_slice(candidate_request03, &initial_reminder_path),
        json_source_slice(&native_request03, &initial_reminder_path),
        "request-03 initial user-context reminder block matches Native raw JSON"
    );
    assert_eq!(
        candidate_request_projection.string_at("/messages/0/content/2/text"),
        Some(INITIAL_REMINDER)
    );
    let expected_occurrences = vec![
        (0, 2, "text".to_owned(), "15000000".to_owned()),
        (2, 0, "content".to_owned(), "14999994".to_owned()),
        (4, 0, "content".to_owned(), "14999991".to_owned()),
    ];
    assert_eq!(
        reminder_occurrences(&native_request_projection),
        expected_occurrences
    );
    assert_eq!(
        reminder_occurrences(&candidate_request_projection),
        expected_occurrences,
        "fixed 5/1, 7/2, 11/3 SSE usage yields the Native initial, previous, and current reminders"
    );

    let result_paths = [
        [
            JsonPathSegment::Key("messages"),
            JsonPathSegment::Index(2),
            JsonPathSegment::Key("content"),
            JsonPathSegment::Index(0),
        ],
        [
            JsonPathSegment::Key("messages"),
            JsonPathSegment::Index(4),
            JsonPathSegment::Key("content"),
            JsonPathSegment::Index(0),
        ],
    ];
    for (index, (path, tool_use_id, reminder)) in [
        (&result_paths[0], TOOL_USE_IDS[0], FIRST_TOOL_REMINDER),
        (&result_paths[1], TOOL_USE_IDS[1], SECOND_TOOL_REMINDER),
    ]
    .into_iter()
    .enumerate()
    {
        let native_block = json_source_slice(&native_request03, path);
        let candidate_block = json_source_slice(candidate_request03, path);
        if candidate_block != native_block {
            let directory = std::env::temp_dir().join(format!(
                "current-alignment-glob-three-requests-{index}-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&directory)
                .expect("create candidate three-request wire diagnostic directory");
            std::fs::write(directory.join("native-tool-result.raw.json"), native_block)
                .expect("save fixed Native ToolResult raw subprojection");
            std::fs::write(
                directory.join("candidate-tool-result.raw.json"),
                candidate_block,
            )
            .expect("save candidate ToolResult raw subprojection");
            std::fs::write(
                directory.join("candidate-request-03.raw.json"),
                candidate_request03,
            )
            .expect("save raw candidate SDK request-03");
            std::fs::copy(&session_path, directory.join("candidate-session.jsonl"))
                .expect("save candidate session JSONL");
            let first_difference = candidate_block
                .iter()
                .zip(native_block)
                .position(|(candidate, native)| candidate != native)
                .unwrap_or(candidate_block.len().min(native_block.len()));
            panic!(
                "Native ToolResult {index} raw bytes differ at {first_difference}; saved Native/candidate subprojections, SDK request, and JSONL at {}",
                directory.display()
            );
        }
        assert_eq!(
            candidate_block, native_block,
            "Native raw ToolResult {index}"
        );
        let pointer = if index == 0 {
            "/messages/2/content/0"
        } else {
            "/messages/4/content/0"
        };
        assert_eq!(
            candidate_request_projection.string_at(&format!("{pointer}/type")),
            Some("tool_result")
        );
        assert_eq!(
            candidate_request_projection.string_at(&format!("{pointer}/tool_use_id")),
            Some(tool_use_id)
        );
        let expected_content = format!("hook-return.fixture\n\n{reminder}");
        assert_eq!(
            candidate_request_projection.string_at(&format!("{pointer}/content")),
            Some(expected_content.as_str())
        );
    }

    let expected_attachment_rows = native_reminder_rows(&case);
    assert_eq!(expected_attachment_rows.len(), 3);
    let expected_reminder_texts = [
        "<total_tokens>15000000 tokens left</total_tokens>",
        "<total_tokens>14999994 tokens left</total_tokens>",
        "<total_tokens>14999991 tokens left</total_tokens>",
    ];
    let native_attachment_projections = expected_attachment_rows
        .iter()
        .map(|line| projection(line))
        .collect::<Vec<_>>();
    for (row, expected_text) in native_attachment_projections
        .iter()
        .zip(expected_reminder_texts)
    {
        assert_eq!(row.string_at("/type"), Some("attachment"));
        assert_eq!(
            row.string_at("/attachment/type"),
            Some("total_tokens_reminder")
        );
        assert_eq!(row.string_at("/attachment/text"), Some(expected_text));
        assert_eq!(
            attachment_keys(row),
            vec!["type".to_owned(), "text".to_owned()]
        );
        assert!(row.at_pointer("/attachment/tool_use_id").is_none());
    }

    let raw_session = std::fs::read(&session_path).expect("read actual candidate JSONL");
    let candidate_rows = raw_session
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let candidate_attachment_rows = candidate_rows
        .iter()
        .filter_map(|line| {
            let row = projection(line);
            (row.string_at("/type") == Some("attachment")
                && row.string_at("/attachment/type") == Some("total_tokens_reminder"))
            .then(|| (line, row))
        })
        .collect::<Vec<_>>();
    assert_eq!(candidate_attachment_rows.len(), 3);
    for (index, ((candidate_line, candidate_row), native_line)) in candidate_attachment_rows
        .iter()
        .zip(expected_attachment_rows.iter())
        .enumerate()
    {
        assert_eq!(
            json_source_slice(candidate_line, &[JsonPathSegment::Key("attachment")]),
            json_source_slice(native_line, &[JsonPathSegment::Key("attachment")]),
            "total_tokens_reminder attachment raw value {index}"
        );
        assert_eq!(
            attachment_keys(candidate_row),
            vec!["type".to_owned(), "text".to_owned()]
        );
        assert!(candidate_row
            .at_pointer("/attachment/tool_use_id")
            .is_none());
        assert!(session::jsonl::uuid::validate_uuid(
            candidate_row
                .string_at("/uuid")
                .expect("candidate attachment UUID")
        ));
        assert!(chrono::DateTime::parse_from_rfc3339(
            candidate_row
                .string_at("/timestamp")
                .expect("candidate attachment timestamp")
        )
        .is_ok());
    }

    let native_tool_rows = [
        std::fs::read(case.join("tool-use-result-row-01.raw.jsonl"))
            .expect("Native first tool-result row"),
        std::fs::read(case.join("tool-use-result-row-02.raw.jsonl"))
            .expect("Native second tool-result row"),
    ];
    for (index, tool_use_id) in TOOL_USE_IDS.iter().enumerate() {
        let native_row = projection(&native_tool_rows[index]);
        let candidate_line = candidate_rows
            .iter()
            .find(|line| {
                let row = projection(line);
                row.string_at("/message/content/0/type") == Some("tool_result")
                    && row.string_at("/message/content/0/tool_use_id") == Some(*tool_use_id)
            })
            .expect("candidate JSONL typed ToolResult row");
        let candidate_row = projection(candidate_line);
        assert_eq!(candidate_row.string_at("/type"), Some("user"));
        assert_eq!(
            candidate_row.string_at("/message/content/0/type"),
            Some("tool_result")
        );
        assert_eq!(
            candidate_row.string_at("/message/content/0/tool_use_id"),
            Some(*tool_use_id)
        );
        assert_eq!(
            candidate_row.string_at("/message/content/0/content"),
            Some("hook-return.fixture")
        );
        assert_eq!(
            json_source_slice(candidate_line, &[JsonPathSegment::Key("toolUseResult")]),
            json_source_slice(
                &native_tool_rows[index],
                &[JsonPathSegment::Key("toolUseResult")]
            ),
            "bare toolUseResult raw object remains separate from typed attachment {index}"
        );
        assert!(candidate_row
            .at_pointer("/toolUseResult/tool_use_id")
            .is_none());
        assert!(session::jsonl::uuid::validate_uuid(
            candidate_row
                .string_at("/uuid")
                .expect("candidate tool-result UUID")
        ));
        assert!(chrono::DateTime::parse_from_rfc3339(
            candidate_row
                .string_at("/timestamp")
                .expect("candidate tool-result timestamp")
        )
        .is_ok());
        assert_eq!(native_row.string_at("/sessionId"), Some(SESSION_UUID));
    }

    let events = output.snapshot().await;
    let tool_result_ids = events
        .iter()
        .filter_map(|event| match event {
            OutputEvent::ToolResult { id, tool, .. } if tool == "Glob" => {
                Some(id.as_str().to_owned())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let expected_tool_result_ids = TOOL_USE_IDS
        .iter()
        .map(|tool_use_id| (*tool_use_id).to_owned())
        .collect::<Vec<_>>();
    assert_eq!(tool_result_ids, expected_tool_result_ids);
}
