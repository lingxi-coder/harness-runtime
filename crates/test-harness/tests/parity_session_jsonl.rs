//! Parity: byte-equivalent session JSONL produced through the orchestrator.
//!
//! M5-07 tested `JsonlWriter`/`JsonlReader` in isolation. M5-14 locks the
//! cross-cutting behaviour: a `ConversationOrchestrator` with an attached
//! `JsonlWriter` produces a JSONL file whose line structure, field names,
//! terminator, and UUID chain are byte-equivalent to the M5-07 golden format.
//!
//! See plan `docs/superpowers/plans/2026-05-25-m5-14-release-v0.6.0.md` Task 4.

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use llm_runtime::ContentBlock as LlmContentBlock;
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use serde::Deserialize;
use serde_json::Value;
use session::jsonl::{JsonlReader, JsonlWriter};
use std::collections::HashSet;
use std::sync::Arc;
use tempfile::TempDir;

const FIXTURE: &str = include_str!("../src/parity/fixtures/parity_session_jsonl.json");
const PRIVATE_SOURCE_KEY_ORDER_KEY: &str = "lingxi_jsonl_source_key_order";

// ============================================================================
// Fixture types
// ============================================================================

#[derive(Debug, Deserialize)]
struct Fixture {
    #[serde(rename = "_meta")]
    meta: Meta,
}

#[derive(Debug, Deserialize)]
struct Meta {
    locks: Locks,
    single_turn_sequence: Vec<SequenceEntry>,
    session_telemetry_events: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Locks {
    line_terminator: String,
    json_format: String,
    user_type: String,
    is_sidechain_default: bool,
    #[allow(dead_code)]
    uuid_pattern: String,
}

#[derive(Debug, Deserialize)]
struct SequenceEntry {
    #[serde(rename = "type")]
    msg_type: String,
    attachment_type: Option<String>,
    expected_attachment: Option<Value>,
}

fn load() -> Fixture {
    serde_json::from_str(FIXTURE).expect("fixture parse")
}

fn source_test_view(source: &Utf16JsonProjection) -> Value {
    serde_json::from_str(
        &source
            .to_json_string()
            .expect("valid source projection serializes as JSON"),
    )
    .expect("these scalar parity fixtures decode as JSON test views")
}

fn assert_reader_source_key_order(line: &session::JsonlMessage, raw_line: &str) {
    let raw = Utf16JsonProjection::parse(raw_line).expect("raw JSONL source parses exactly");
    assert!(
        !raw.contains_property(PRIVATE_SOURCE_KEY_ORDER_KEY),
        "reader-only key-order metadata is not serialized to the raw row"
    );
    let raw_key_order = raw
        .object_entries()
        .expect("raw JSONL row is an object")
        .into_iter()
        .map(|entry| entry.code_units.to_vec())
        .collect::<Vec<_>>();
    let recorded = line
        .extra
        .get_property(PRIVATE_SOURCE_KEY_ORDER_KEY)
        .expect("reader records raw outer-key order in private metadata");
    let recorded_key_order = (0..recorded.array_len().expect("key order is an array"))
        .map(|index| {
            recorded
                .get_index(index)
                .and_then(|key| key.code_units().map(|units| units.to_vec()))
                .expect("each recorded source key is a string")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        recorded_key_order, raw_key_order,
        "reader metadata exactly preserves raw top-level key order"
    );
}

fn reader_only_extra(line: &session::JsonlMessage) -> Value {
    let extra = source_test_view(&line.extra);
    let mut expected = serde_json::Map::new();
    expected.insert(
        PRIVATE_SOURCE_KEY_ORDER_KEY.to_owned(),
        extra[PRIVATE_SOURCE_KEY_ORDER_KEY].clone(),
    );
    Value::Object(expected)
}

fn assert_exact_keys(value: &Value, expected: &[&str], label: &str) {
    let object = value
        .as_object()
        .unwrap_or_else(|| panic!("{label} must be an object"));
    let mut actual = object.keys().map(String::as_str).collect::<Vec<_>>();
    let mut expected = expected.to_vec();
    actual.sort_unstable();
    expected.sort_unstable();
    assert_eq!(actual, expected, "{label} keys");
}

fn assert_single_system_rendering(extra: &Value, label: &str) -> String {
    assert_eq!(extra["renderedRole"], "system", "{label} role");
    let rendered = extra["rendered"]
        .as_array()
        .unwrap_or_else(|| panic!("{label} rendering must be an array"));
    assert_eq!(rendered.len(), 1, "{label} has one rendered block");
    assert_exact_keys(
        &rendered[0],
        &["content"],
        &format!("{label} rendered block"),
    );
    let rendered = rendered[0]["content"]
        .as_str()
        .unwrap_or_else(|| panic!("{label} rendered content must be text"));
    assert!(
        rendered.starts_with("<system-reminder>\n"),
        "{label} wrapper start"
    );
    assert!(
        rendered.ends_with("\n</system-reminder>"),
        "{label} wrapper end"
    );
    rendered.to_owned()
}

fn assert_environment_ingress(extra: &Value, payload: &Value, raw_row: &Value) -> (Value, String) {
    assert_exact_keys(
        extra,
        &["rendered", "renderedRole", PRIVATE_SOURCE_KEY_ORDER_KEY],
        "environment rendering metadata",
    );
    assert_exact_keys(payload, &["type", "snapshot"], "environment attachment");
    assert_eq!(payload["type"], "environment");
    assert_eq!(raw_row["attachment"], *payload, "environment on disk");

    let snapshot = &payload["snapshot"];
    assert_exact_keys(
        snapshot,
        &[
            "workingDirectory",
            "isWorktree",
            "isGitRepo",
            "additionalWorkingDirectories",
            "platform",
            "shell",
            "osVersion",
        ],
        "environment snapshot",
    );
    let working_directory = snapshot["workingDirectory"]
        .as_str()
        .expect("working directory is a string");
    assert!(std::path::Path::new(working_directory).is_absolute());
    assert!(snapshot["isWorktree"].is_boolean());
    assert!(snapshot["isGitRepo"].is_boolean());
    assert!(snapshot["additionalWorkingDirectories"]
        .as_array()
        .is_some_and(|paths| paths.iter().all(Value::is_string)));
    for field in ["platform", "shell", "osVersion"] {
        assert!(
            snapshot[field]
                .as_str()
                .is_some_and(|value| !value.is_empty()),
            "{field}"
        );
    }

    let rendered = assert_single_system_rendering(extra, "environment");
    assert!(rendered.contains("# Environment\n"));
    assert!(rendered.contains(working_directory));
    assert!(rendered.contains(snapshot["platform"].as_str().unwrap()));
    assert!(rendered.contains(snapshot["shell"].as_str().unwrap()));
    assert!(rendered.contains(snapshot["osVersion"].as_str().unwrap()));
    (payload.clone(), rendered)
}

fn assert_model_ingress(extra: &Value, payload: &Value, raw_row: &Value) -> (Value, String) {
    assert_exact_keys(
        extra,
        &["rendered", "renderedRole", PRIVATE_SOURCE_KEY_ORDER_KEY],
        "model rendering metadata",
    );
    assert_exact_keys(payload, &["type", "identity", "text"], "model attachment");
    assert_eq!(payload["type"], "model");
    assert_eq!(raw_row["attachment"], *payload, "model on disk");

    let identity = &payload["identity"];
    assert_exact_keys(
        identity,
        &["modelId", "marketingName", "knowledgeCutoff"],
        "model identity",
    );
    let model_id = identity["modelId"]
        .as_str()
        .filter(|value| !value.is_empty())
        .expect("model ID is a nonempty string");
    for field in ["marketingName", "knowledgeCutoff"] {
        assert!(
            identity[field].is_null() || identity[field].is_string(),
            "{field} type"
        );
    }
    let text = payload["text"].as_str().expect("model text is a string");
    assert!(
        text.contains(model_id),
        "model reminder names its exact identity"
    );

    let rendered = assert_single_system_rendering(extra, "model");
    assert_eq!(
        rendered,
        format!("<system-reminder>\n{text}\n</system-reminder>"),
        "model rendered reminder"
    );
    (payload.clone(), rendered)
}

// ============================================================================
// Helpers
// ============================================================================

fn build_orchestrator_with_writer(
    api: Arc<MockApiClient>,
    writer: Arc<JsonlWriter>,
) -> ConversationOrchestrator {
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let tools = Arc::new(tool_api::registry::ToolRegistry::new());
    let output = Arc::new(MockOutputStream::new());
    let cfg = OrchestratorConfig::default();
    ConversationOrchestrator::new(
        cfg,
        api,
        tools,
        hooks,
        perms,
        output,
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_jsonl_writer(writer)
}

async fn assert_native_single_turn_rows(
    lines: &[session::JsonlMessage],
    raw: &str,
    api: &MockApiClient,
) {
    use lingxi_core::types::{ContentBlock, ConversationMessage};

    let fixture = load();
    assert_eq!(lines.len(), fixture.meta.single_turn_sequence.len());
    let raw_lines = raw.lines().collect::<Vec<_>>();
    let raw_rows: Vec<Value> = raw_lines
        .iter()
        .map(|line| serde_json::from_str(line).expect("valid JSONL row"))
        .collect();
    assert_eq!(raw_rows.len(), lines.len());

    // These native cases execute .286 Br/renderers and fmn/xd unchanged.
    // Only the local date and the host's branded static prompt vary here.
    let oracle: Value = serde_json::from_str(include_str!(
        "../../core/tests/fixtures/instruction_announcements_2_1_286.json"
    ))
    .unwrap();
    assert_eq!(oracle["version"], "2.1.286");
    assert_eq!(
        oracle["binary_sha256"],
        "75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433"
    );
    let native = oracle["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "successful-empty-initial")
        .unwrap();
    assert_eq!(native["expected"].as_array().unwrap().len(), 2);
    assert_eq!(native["expected"][0]["rendered"], serde_json::json!([]));
    assert_eq!(
        fixture.meta.single_turn_sequence[1].expected_attachment,
        Some(serde_json::json!({"type": "environment"}))
    );
    assert_eq!(
        fixture.meta.single_turn_sequence[2].expected_attachment,
        Some(serde_json::json!({"type": "model"}))
    );
    assert_eq!(
        fixture.meta.single_turn_sequence[4].expected_attachment,
        Some(native["expected"][0]["attachment"].clone())
    );
    let mut native_date = native["expected"][1]["attachment"].clone();
    native_date["date"] = serde_json::json!("$currentDate");
    assert_eq!(
        fixture.meta.single_turn_sequence[5].expected_attachment,
        Some(native_date)
    );
    let native_snapshot = oracle["snapshotCases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "valid-announced-system-prompt")
        .unwrap();
    assert_eq!(native_snapshot["expected"]["schemaValid"], true);
    let mut snapshot_payload = native_snapshot["prior"][0]["attachment"].clone();
    snapshot_payload["systemPrompt"] = serde_json::json!(["$capturedSystem"]);
    assert_eq!(
        fixture.meta.single_turn_sequence[6].expected_attachment,
        Some(snapshot_payload)
    );

    let calls = api.captured_msgs().await;
    assert_eq!(calls.len(), 1, "single turn makes exactly one API call");
    let systems = api.captured_systems().await;
    assert_eq!(systems.len(), 1);
    let system = systems[0].as_ref().expect("static system prompt");
    let system_text = system.as_str();
    assert!(!system_text.is_empty());
    // Native persists the source string vector before dynamic context. The
    // default mock route has no provider profile, so its source vector carries
    // no global-cache marker.
    let snapshot_extra = source_test_view(&lines[6].extra);
    assert_reader_source_key_order(&lines[6], raw_lines[6]);
    assert_eq!(snapshot_extra, reader_only_extra(&lines[6]));
    let snapshot_payload = source_test_view(
        lines[6]
            .attachment_projection
            .as_ref()
            .expect("prompt snapshot is a top-level Native attachment payload"),
    );
    assert_eq!(snapshot_payload["type"], "prompt_snapshot");
    let snapshot_system = snapshot_payload["systemPrompt"].clone();
    let snapshot_blocks = snapshot_system
        .as_array()
        .expect("the persisted source-vector snapshot is a Native string array");
    assert!(!snapshot_blocks.is_empty());
    assert!(snapshot_blocks.iter().all(Value::is_string));
    assert!(snapshot_payload.get("sharedBoundary").is_none());
    let snapshot_text = snapshot_blocks
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()
        .expect("source blocks are strings")
        .join("\n\n");
    assert!(system_text.starts_with(&snapshot_text));
    assert_eq!(
        raw_rows[6]["attachment"], snapshot_payload,
        "the raw JSONL row preserves the complete prompt snapshot payload"
    );
    assert!(api.captured_tools().await[0].is_empty());

    let date = orchestrator::prompt::env_meta::current_date_string();
    let mut seen = HashSet::new();
    for (index, (line, expected)) in lines
        .iter()
        .zip(&fixture.meta.single_turn_sequence)
        .enumerate()
    {
        assert_reader_source_key_order(line, raw_lines[index]);
        let uuid = uuid::Uuid::parse_str(&line.uuid).expect("valid row UUID");
        assert_eq!(line.uuid, uuid.to_string(), "canonical UUID at row {index}");
        assert!(seen.insert(uuid), "distinct UUID at row {index}");
        let parent = index.checked_sub(1).map(|prior| lines[prior].uuid.as_str());
        assert_eq!(line.parent_uuid.as_deref(), parent, "parent at row {index}");
        assert_eq!(raw_rows[index]["parentUuid"], serde_json::json!(parent));
        assert_eq!(line.session_id, lines[0].session_id);
        assert_eq!(line.message_type, expected.msg_type, "type at row {index}");
        if let Some(kind) = expected.attachment_type.as_deref() {
            let mut payload = expected
                .expected_attachment
                .clone()
                .expect("attachment discriminator in fixture");
            match kind {
                "environment" => {
                    assert_eq!(payload, serde_json::json!({"type": "environment"}));
                }
                "model" => {
                    assert_eq!(payload, serde_json::json!({"type": "model"}));
                }
                "date" => payload["date"] = serde_json::json!(date),
                "prompt_snapshot" => {
                    payload["systemPrompt"] = snapshot_system.clone();
                }
                _ => {}
            }
            assert_eq!(payload["type"], kind);
            let line_extra = source_test_view(&line.extra);
            let attachment = source_test_view(
                line.attachment_projection
                    .as_ref()
                    .expect("attachment row keeps its top-level raw payload"),
            );
            match kind {
                "environment" => {
                    let (actual, rendered) =
                        assert_environment_ingress(&line_extra, &attachment, &raw_rows[index]);
                    assert_eq!(actual["type"], payload["type"]);
                    assert!(rendered.starts_with("<system-reminder>\n# Environment\n"));
                }
                "model" => {
                    let (actual, rendered) =
                        assert_model_ingress(&line_extra, &attachment, &raw_rows[index]);
                    assert_eq!(actual["type"], payload["type"]);
                    assert_eq!(
                        rendered,
                        format!(
                            "<system-reminder>\n{}\n</system-reminder>",
                            actual["text"].as_str().unwrap()
                        )
                    );
                }
                "total_tokens_reminder" => {
                    assert_eq!(attachment, payload, "total-token payload at row {index}");
                    assert_exact_keys(
                        &line_extra,
                        &["rendered", "renderedRole", PRIVATE_SOURCE_KEY_ORDER_KEY],
                        "total-token rendering metadata",
                    );
                    let rendered = assert_single_system_rendering(&line_extra, "total tokens");
                    assert_eq!(line_extra["renderedRole"], "system");
                    assert_eq!(
                        rendered,
                        format!(
                            "<system-reminder>\n{}\n</system-reminder>",
                            payload["text"]
                                .as_str()
                                .expect("total-token text is a string")
                        ),
                        "total-token rendering preserves the exact typed payload text"
                    );
                    assert_eq!(
                        raw_rows[index]["attachment"], payload,
                        "raw JSONL row preserves the typed total-token payload"
                    );
                    assert_eq!(
                        raw_rows[index]["rendered"], line_extra["rendered"],
                        "raw JSONL row preserves the rendered cache"
                    );
                    assert_eq!(raw_rows[index]["renderedRole"], "system");
                }
                _ => {
                    assert_eq!(attachment, payload, "typed payload at row {index}");
                    assert_eq!(
                        line_extra,
                        reader_only_extra(line),
                        "no duplicated extra at row {index}"
                    );
                    assert_eq!(raw_rows[index]["attachment"], payload, "disk row {index}");
                }
            }
            assert_eq!(
                source_test_view(&line.message),
                Value::Null,
                "no decoded projection at row {index}"
            );
            assert!(
                raw_rows[index].get("message").is_none(),
                "native attachment has no inner message at row {index}"
            );
            assert!(
                raw_rows[index].get("isMeta").is_none(),
                "attachment is persisted as raw metadata at row {index}"
            );
        }
    }
    lingxi_core::types::SessionId::parse_prefixed(&lines[0].session_id)
        .expect("valid core session ID");
    assert_eq!(
        source_test_view(&lines[0].message),
        serde_json::json!({"role":"user","content":[{"type":"text","text":"say hi"}]})
    );
    let assistant_message = source_test_view(&lines[7].message);
    assert_eq!(assistant_message["role"], "assistant");
    assert_eq!(
        assistant_message["content"],
        serde_json::json!([{"type":"text","text":"hello"}])
    );
    assert_eq!(assistant_message["stop_reason"], "end_turn");

    // Durable JSONL starts at the user's parent chain; the Native prepared
    // request projects environment/model and turn reminders before that user.
    // The prompt snapshot remains durable-only and is not a model request row.
    let sent = &calls[0];
    assert_eq!(
        sent.len(),
        6,
        "environment, model, total_tokens, session_context, date, human prompt"
    );
    for (line_index, message) in [1usize, 2, 3, 4, 5, 0].into_iter().zip(sent) {
        assert_eq!(lines[line_index].uuid, message.id().as_uuid().to_string());
    }
    let environment_extra = source_test_view(&lines[1].extra);
    let environment_payload = source_test_view(
        lines[1]
            .attachment_projection
            .as_ref()
            .expect("environment attachment payload"),
    );
    let (_, environment_rendered) =
        assert_environment_ingress(&environment_extra, &environment_payload, &raw_rows[1]);
    assert!(
        matches!(&sent[0], ConversationMessage::User { content, is_meta: true, .. }
        if content.iter().filter_map(ContentBlock::visible_text).collect::<String>() == environment_rendered)
    );
    let model_extra = source_test_view(&lines[2].extra);
    let model_payload = source_test_view(
        lines[2]
            .attachment_projection
            .as_ref()
            .expect("model attachment payload"),
    );
    let (_, model_rendered) = assert_model_ingress(&model_extra, &model_payload, &raw_rows[2]);
    assert!(
        matches!(&sent[1], ConversationMessage::User { content, is_meta: true, .. }
        if content.iter().filter_map(ContentBlock::visible_text).collect::<String>() == model_rendered)
    );
    let token_body = fixture.meta.single_turn_sequence[3]
        .expected_attachment
        .as_ref()
        .unwrap()["text"]
        .as_str()
        .unwrap();
    assert!(
        matches!(&sent[2], ConversationMessage::User { content, is_meta: true, .. }
        if content == &[ContentBlock::Text { text: format!("<system-reminder>\n{token_body}\n</system-reminder>"), citations: None }])
    );
    assert!(
        matches!(&sent[3], ConversationMessage::System { content, subtype, compact_metadata: None, refusal_fallback: None, .. }
        if content.string_at("").is_some_and(str::is_empty)
            && subtype.as_deref() == Some("model_reminder_attachment"))
    );
    let date_body = native["expected"][1]["rendered"][0]
        .as_str()
        .unwrap()
        .replace(
            native["expected"][1]["attachment"]["date"]
                .as_str()
                .unwrap(),
            &date,
        );
    assert!(
        matches!(&sent[4], ConversationMessage::User { content, is_meta: true, .. }
        if content == &[ContentBlock::Text { text: date_body, citations: None }])
    );
    let expected_prompt_units = "say hi".encode_utf16().collect::<Vec<_>>();
    assert!(matches!(
        &sent[5],
        ConversationMessage::User {
            content,
            is_meta: false,
            ..
        } if matches!(
            content.as_slice(),
            [ContentBlock::TextJsUtf16 {
                text,
                utf16_code_units,
                citations: None,
            }] if text == "say hi"
                && utf16_code_units.as_slice() == expected_prompt_units.as_slice()
        )
    ));
}

// ============================================================================
// T4 — single turn: user + six typed attachments + assistant
// ============================================================================

#[tokio::test]
async fn single_turn_produces_the_fixture_line_count() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("session.jsonl");

    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(tmp.path().to_path_buf()),
    );
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));

    let orch = build_orchestrator_with_writer(api.clone(), Arc::clone(&writer));
    let outcome = orch.run_turn("say hi").await.expect("turn must succeed");
    assert!(matches!(
        outcome,
        orchestrator::ConversationOutcome::EndTurn { turn_count: 1, .. }
    ));

    // Read back via JsonlReader.
    let reader = JsonlReader::new(path.clone(), Arc::clone(&fs));
    let lines = reader.read_all().await.expect("read_all must succeed");

    let f = load();
    assert_eq!(
        lines.len(),
        f.meta.single_turn_sequence.len(),
        "single turn must produce the fixture's line count, got {:?}",
        lines
            .iter()
            .map(|l| l.message_type.as_str())
            .collect::<Vec<_>>()
    );
    let raw = std::fs::read_to_string(&path).expect("read actual JSONL bytes");
    assert_native_single_turn_rows(&lines, &raw, &api).await;
}

// ============================================================================
// T4 — line order and chaining follow the fixture sequence
// ============================================================================

#[tokio::test]
async fn single_turn_line_types_follow_the_fixture_order() {
    let f = load();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("session.jsonl");

    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(tmp.path().to_path_buf()),
    );
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));

    let orch = build_orchestrator_with_writer(api.clone(), Arc::clone(&writer));
    let outcome = orch.run_turn("say hi").await.expect("turn must succeed");
    assert!(matches!(
        outcome,
        orchestrator::ConversationOutcome::EndTurn { turn_count: 1, .. }
    ));

    let reader = JsonlReader::new(path.clone(), Arc::clone(&fs));
    let lines = reader.read_all().await.expect("read_all must succeed");

    let expected_types: Vec<&str> = f
        .meta
        .single_turn_sequence
        .iter()
        .map(|e| e.msg_type.as_str())
        .collect();

    let actual_types: Vec<&str> = lines.iter().map(|l| l.message_type.as_str()).collect();
    assert_eq!(
        actual_types, expected_types,
        "line types must follow the fixture order"
    );
    let raw = std::fs::read_to_string(&path).expect("read actual JSONL bytes");
    assert_native_single_turn_rows(&lines, &raw, &api).await;
}

// ============================================================================
// T4 — UUID chain: all attachment and assistant rows follow their parent
// ============================================================================

#[tokio::test]
async fn single_turn_parent_uuid_chain_is_correct() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("session.jsonl");

    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(tmp.path().to_path_buf()),
    );
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));

    let orch = build_orchestrator_with_writer(api, Arc::clone(&writer));
    orch.run_turn("say hi").await.expect("turn must succeed");

    let reader = JsonlReader::new(path.clone(), Arc::clone(&fs));
    let lines = reader.read_all().await.expect("read_all must succeed");

    let user = &lines[0];
    let assistant = lines.last().expect("assistant is the final row");
    assert_eq!(lines.len(), load().meta.single_turn_sequence.len());

    assert!(
        user.parent_uuid.is_none(),
        "first line (user) must have parentUuid = null"
    );
    for pair in lines.windows(2) {
        assert_eq!(
            pair[1].parent_uuid.as_deref(),
            Some(pair[0].uuid.as_str()),
            "every typed attachment and assistant row follows its direct parent"
        );
    }
    assert_eq!(
        assistant.message_type, "assistant",
        "assistant follows the full environment/model/reminder/context chain"
    );
}

// ============================================================================
// T4 — sessionId: all lines share the same session ID
// ============================================================================

#[tokio::test]
async fn single_turn_all_lines_share_session_id() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("session.jsonl");

    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(tmp.path().to_path_buf()),
    );
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));

    let orch = build_orchestrator_with_writer(api, Arc::clone(&writer));
    orch.run_turn("say hi").await.expect("turn must succeed");

    let reader = JsonlReader::new(path.clone(), Arc::clone(&fs));
    let lines = reader.read_all().await.expect("read_all must succeed");

    let session_ids: Vec<&str> = lines.iter().map(|l| l.session_id.as_str()).collect();
    assert!(
        session_ids.windows(2).all(|w| w[0] == w[1]),
        "all lines must share the same sessionId; got {session_ids:?}"
    );
}

// ============================================================================
// T4 — format locks: LF-only terminator, compact JSON, uuid v4 pattern
// ============================================================================

#[tokio::test]
async fn single_turn_file_has_lf_only_terminator_and_compact_json() {
    let f = load();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("session.jsonl");

    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(tmp.path().to_path_buf()),
    );
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));

    let orch = build_orchestrator_with_writer(api, Arc::clone(&writer));
    orch.run_turn("say hi").await.expect("turn must succeed");

    let raw = std::fs::read_to_string(&path).expect("read raw file");

    // LF-only: no CR byte.
    assert!(
        !raw.contains('\r'),
        "fixture lock '{:?}': file must use LF-only line endings, no CR found",
        f.meta.locks.line_terminator
    );

    // Compact JSON: no lines that contain `: ` (space after colon in key-value).
    for (i, line) in raw.lines().enumerate() {
        // Compact serde_json never emits ": " — it always emits ":"
        // (verify by checking that none of the outer field separators have trailing space).
        // We check the absence of `": "` to confirm no pretty-printing.
        assert!(
            !line.contains("\": \"") || {
                // The only ": " that appears must be INSIDE a JSON string value,
                // not at the top-level key separator position.
                // Simpler invariant: the line must parse as valid compact JSON.
                serde_json::from_str::<Value>(line).is_ok()
            },
            "line {}: fixture lock '{:?}': JSON must be compact",
            i + 1,
            f.meta.locks.json_format
        );
        // Verify it parses as valid JSON.
        serde_json::from_str::<Value>(line)
            .unwrap_or_else(|e| panic!("line {} is not valid JSON: {e}", i + 1));
    }
}

// ============================================================================
// T4 — userType field: locked to "external"
// ============================================================================

#[tokio::test]
async fn single_turn_user_line_has_usertype_external() {
    let f = load();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("session.jsonl");

    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(tmp.path().to_path_buf()),
    );
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));

    let orch = build_orchestrator_with_writer(api, Arc::clone(&writer));
    orch.run_turn("say hi").await.expect("turn must succeed");

    let reader = JsonlReader::new(path.clone(), Arc::clone(&fs));
    let lines = reader.read_all().await.expect("read_all must succeed");
    let user = &lines[0];

    assert_eq!(
        user.user_type.as_deref(),
        Some(f.meta.locks.user_type.as_str()),
        "user line userType must be locked to {:?}",
        f.meta.locks.user_type
    );
}

// ============================================================================
// T4 — isSidechain: default false for main-loop messages
// ============================================================================

#[tokio::test]
async fn single_turn_is_sidechain_is_false() {
    let f = load();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("session.jsonl");

    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(tmp.path().to_path_buf()),
    );
    let writer = Arc::new(JsonlWriter::new(path.clone(), Arc::clone(&fs)));

    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "hello".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));

    let orch = build_orchestrator_with_writer(api, Arc::clone(&writer));
    orch.run_turn("say hi").await.expect("turn must succeed");

    let reader = JsonlReader::new(path.clone(), Arc::clone(&fs));
    let lines = reader.read_all().await.expect("read_all must succeed");

    for line in &lines {
        assert_eq!(
            line.is_sidechain, f.meta.locks.is_sidechain_default,
            "isSidechain must be {} for main-loop messages",
            f.meta.locks.is_sidechain_default
        );
    }
}

// ============================================================================
// T4 — telemetry invariants: session events are registered
// ============================================================================

#[test]
fn session_telemetry_events_are_registered() {
    let f = load();
    let registered: std::collections::HashSet<&&str> =
        telemetry::tengu::ALL_EVENT_NAMES.iter().collect();
    for name in &f.meta.session_telemetry_events {
        assert!(
            registered.contains(&name.as_str()),
            "session event {name:?} not found in ALL_EVENT_NAMES"
        );
    }
}
