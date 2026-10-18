//! Focused Native 2.1.295 session.append contract gate.
//!
//! The system case uses the checked-in Native plugin-test module. The user
//! lone-surrogate case uses a source-derived candidate module with a stable
//! typed-message UUID. Tests observe actual ModHost dispatch and the retained
//! transcript consumer as separate boundaries.

use agent::transcript::{AgentTranscriptWriter, TranscriptEntry};
use hooks::mods::ModHost;
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection, AgentId, ContentBlock, ConversationMessage, MessageId,
};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const CASES: [(&str, &str); 4] = [
    ("rewrite", "00000000-0000-4000-8000-000000000001"),
    ("direct", "00000000-0000-4000-8000-000000000002"),
    ("cancel", "00000000-0000-4000-8000-000000000003"),
    ("throw", "00000000-0000-4000-8000-000000000004"),
];
const USER_UTF16_UUID: &str = "00000000-0000-4000-8000-000000000016";

fn case_id(uuid: &str) -> MessageId {
    MessageId::from_uuid(Uuid::parse_str(uuid).expect("fixed case UUID"))
}

fn user_with_utf16_text(id: MessageId, utf16_code_units: Vec<u16>) -> ConversationMessage {
    ConversationMessage::User {
        id,
        content: vec![ContentBlock::TextJsUtf16 {
            text: String::from_utf16_lossy(&utf16_code_units),
            utf16_code_units,
            citations: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
        turn_companion: None,
        usage_limit_note: None,
        origin: None,
        turn_origin: None,
        interrupted_by_shutdown: None,
        refused_message_id: None,
        api_message_override: None,
    }
}

fn row_text(message: &ConversationMessage) -> String {
    message.text_content()
}

fn projection_json(projection: &Utf16JsonProjection) -> Value {
    serde_json::from_str(&projection.to_json_string().expect("projection JSON"))
        .expect("projection is valid JSON")
}

#[tokio::test]
async fn native_session_append_middleware_contract_reaches_host_and_transcript() {
    let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/parity/current-alignment/fixtures/mod-middleware");
    let module = fixture_root.join("hooks/register.js");
    let host = ModHost::start(None).await.expect("start actual Mod worker");
    host.load(
        "current-alignment-middleware",
        &fixture_root,
        &module,
        Utf16JsonProjection::parse("{}").expect("empty Mod options JSON"),
    )
    .await
    .expect("load checked-in Native fixture module");

    let mut worker_stage = Vec::new();
    for (case, uuid) in CASES {
        let id = case_id(uuid);
        let input = json!({
            "message": {"type":"user", "content":[{"type":"text", "text":"fixture-input"}]},
            "door":"user",
            "origin":{"kind":"core"},
            "uuid":id.as_uuid().to_string(),
        });
        let core_calls = Arc::new(Mutex::new(Vec::<Utf16JsonProjection>::new()));
        let observed = core_calls.clone();
        let result = host
            .dispatch(
                "session.append",
                Utf16JsonProjection::from_serde_value(&input)
                    .expect("fixed session.append input projection"),
                move |event| {
                    let observed = observed.clone();
                    async move {
                        let event = event.expect("session.append next carries an event");
                        observed.lock().expect("core call lock").push(event.clone());
                        Ok(Some(
                            Utf16JsonProjection::object(vec![
                                (
                                    "message".into(),
                                    event.subprojection("/message").expect("message projection"),
                                ),
                                (
                                    "uuid".into(),
                                    event.subprojection("/uuid").expect("uuid projection"),
                                ),
                            ])
                            .expect("append core result projection"),
                        ))
                    }
                },
            )
            .await
            .expect("ModHost dispatch");
        let core_events = core_calls.lock().expect("core call lock").clone();
        worker_stage.push(json!({
            "case":case,
            "core_next_inputs":core_events.iter().map(projection_json).collect::<Vec<_>>(),
            "result":projection_json(&result),
        }));
    }

    let registry = {
        let mut registry = hooks::HookRegistry::new();
        registry.set_mod_host(host.clone());
        registry
    };
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    let temp = tempfile::tempdir().expect("transcript temp directory");
    let transcript_path = temp.path().join("agent.jsonl");
    let writer = AgentTranscriptWriter::new(
        transcript_path.clone(),
        AgentId::new(),
        Arc::new(platform_posix::PosixFileSystem::new(
            temp.path().to_path_buf(),
        )),
    )
    .with_mod_append(
        Some(executor),
        temp.path().to_path_buf(),
        Some("test-model".into()),
    );

    let mut terminal_rows = Vec::new();
    for (case, uuid) in CASES {
        let mut message = ConversationMessage::user(case_id(uuid), "fixture-input".into());
        writer
            .record_retained(&mut message, &[])
            .await
            .expect("persist retained row through current session.append path");
        terminal_rows.push(json!({"case":case,"text":row_text(&message)}));
    }

    let persisted = std::fs::read_to_string(&transcript_path).expect("read transcript JSONL");
    let persisted_rows = persisted
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("transcript JSONL row"))
        .map(|row| row["message"]["content"][0]["text"].clone())
        .collect::<Vec<_>>();
    let expected_texts = vec![
        json!("native-prefix:fixture-input"),
        json!("fixture-input"),
        json!("fixture-input"),
        json!("fixture-input"),
    ];
    assert_eq!(
        terminal_rows
            .iter()
            .map(|row| row["text"].clone())
            .collect::<Vec<_>>(),
        expected_texts,
        "the in-memory retained rows match Native's public append results"
    );
    assert_eq!(persisted_rows.len(), 4);
    assert_eq!(
        persisted_rows, expected_texts,
        "the transcript consumer persists the same Native-aligned rows"
    );

    let direct = worker_stage
        .iter()
        .find(|row| row["case"] == "direct")
        .unwrap();
    assert!(direct["core_next_inputs"].as_array().unwrap().is_empty());
    assert_eq!(
        direct["result"]["message"]["content"][0]["text"],
        "native-direct-return"
    );
    for case in ["rewrite", "cancel", "throw"] {
        let row = worker_stage.iter().find(|row| row["case"] == case).unwrap();
        assert_eq!(
            row["core_next_inputs"].as_array().unwrap().len(),
            1,
            "{case}"
        );
    }

    println!(
        "CURRENT_ALIGNMENT_MOD_APPEND={}",
        json!({"worker_stage":worker_stage,"transcript_terminal":terminal_rows,
            "persisted_texts":persisted_rows})
    );
}

#[tokio::test]
async fn native_system_lone_session_append_reaches_bun_worker_and_host_writer() {
    const SYSTEM_UUID: &str = "00000000-0000-4000-8000-000000000015";
    let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/parity/current-alignment/fixtures/session-append/native-plugin");
    let native_event_path = fixture_root
        .join("native-baseline/2.1.295/system-content-lone-surrogate/result-event.raw.json");
    let native_event_raw = std::fs::read_to_string(&native_event_path)
        .expect("captured public Native system append event");
    let native_event = Utf16JsonProjection::parse(&native_event_raw)
        .expect("raw Native event retains escaped lone UTF-16 unit");
    let native_input = native_event
        .subprojection("/input")
        .expect("Native input projection");
    let native_result = native_event
        .subprojection("/result")
        .expect("Native result projection");
    let input_units = native_input
        .string_units("/message/content/0/text")
        .expect("Native input text units");
    let expected_result_units = native_result
        .string_units("/message/content/0/text")
        .expect("Native rewritten result text units");
    assert_eq!(native_input.string_at("/message/type"), Some("system"));
    assert_eq!(native_input.string_at("/uuid"), Some(SYSTEM_UUID));
    assert!(input_units.contains(&0xd800));
    assert!(expected_result_units.contains(&0xd800));

    // This direct dispatch is the actual Bun worker seam. It checks that the
    // worker receives and returns the same system-message text as Native.
    let module = fixture_root.join("hooks/register.js");
    let host = ModHost::start(None).await.expect("start actual Mod worker");
    host.load(
        "current-alignment-session-append",
        &fixture_root,
        &module,
        Utf16JsonProjection::parse("{}").expect("empty Mod options JSON"),
    )
    .await
    .expect("load the Native system append module into Bun");
    let forwarded_rows = Arc::new(Mutex::new(Vec::<Utf16JsonProjection>::new()));
    let captured = forwarded_rows.clone();
    let worker_result = host
        .dispatch("session.append", native_input.clone(), move |event| {
            let captured = captured.clone();
            async move {
                let event = event.expect("system append next carries its event");
                captured
                    .lock()
                    .expect("forwarded-row lock")
                    .push(event.clone());
                Ok(Some(
                    Utf16JsonProjection::object(vec![
                        (
                            "message".into(),
                            event.subprojection("/message").expect("message projection"),
                        ),
                        (
                            "uuid".into(),
                            event.subprojection("/uuid").expect("uuid projection"),
                        ),
                    ])
                    .expect("system append continuation result"),
                ))
            }
        })
        .await
        .expect("dispatch system append through Bun");
    assert_eq!(
        worker_result.string_units("/message/content/0/text"),
        Some(expected_result_units.clone()),
        "the production worker's exact source units match the Native result event"
    );
    assert_eq!(worker_result.string_at("/message/type"), Some("system"));
    assert_eq!(worker_result.string_at("/uuid"), Some(SYSTEM_UUID));
    let forwarded = forwarded_rows
        .lock()
        .expect("forwarded-row lock")
        .first()
        .cloned()
        .expect("Native rewrite reaches the actual continuation");
    assert_eq!(
        forwarded.string_units("/message/content/0/text"),
        Some(expected_result_units.clone())
    );

    // The second stage reaches AgentTranscriptWriter's real retained-row
    // caller. This is a Host persistence seam, not a full Agent turn or model
    // request. The Host row has a flat `System.content` value, unlike Native's
    // plugin event `{content:[{type,text}]}`, so compare exact text units and
    // row fields instead of claiming whole-event byte equality.
    let registry = {
        let mut registry = hooks::HookRegistry::new();
        registry.set_mod_host(host);
        registry
    };
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    let temp = tempfile::tempdir().expect("isolated transcript directory");
    let transcript_path = temp.path().join("agent.jsonl");
    let writer = AgentTranscriptWriter::new(
        transcript_path.clone(),
        AgentId::new(),
        Arc::new(platform_posix::PosixFileSystem::new(
            temp.path().to_path_buf(),
        )),
    )
    .with_mod_append(
        Some(executor),
        temp.path().to_path_buf(),
        Some("test-model".into()),
    );
    let display = String::from_utf16_lossy(&input_units);
    let mut system_message = ConversationMessage::System {
        id: case_id(SYSTEM_UUID),
        content: Utf16JsonProjection::root_string(display, input_units)
            .expect("fixed system source content"),
        subtype: None,
        api_system: None,
        compact_metadata: None,
        model_fallback: None,
        refusal_fallback: None,
    };
    writer
        .record_retained(&mut system_message, &[])
        .await
        .expect("persist retained system row through AgentTranscriptWriter");
    let ConversationMessage::System { content, .. } = &system_message else {
        panic!("retained message remains a system row")
    };
    assert_eq!(
        content.string_units(""),
        Some(expected_result_units.clone()),
        "Host cache and transcript value receive the Native system rewrite"
    );

    let persisted = std::fs::read(&transcript_path).expect("read retained system JSONL");
    assert!(persisted
        .windows(b"\\ud800".len())
        .any(|window| window == b"\\ud800"));
    assert!(!persisted
        .windows(3)
        .any(|window| window == [0xef, 0xbf, 0xbd]));
    let persisted_line = std::str::from_utf8(&persisted)
        .expect("JSONL has UTF-8 framing with escaped lone unit")
        .lines()
        .next()
        .expect("one persisted system row");
    let persisted_projection = Utf16JsonProjection::parse(persisted_line)
        .expect("persisted row parses as exact UTF-16 JSON projection");
    // JSONL stores the typed ConversationMessage (`role`-tagged) inside the
    // TranscriptEntry; it does not persist the Native plugin event envelope.
    let persisted_entry = serde_json::from_str::<TranscriptEntry>(persisted_line)
        .expect("persisted JSONL row matches the current TranscriptEntry schema");
    assert_eq!(
        persisted_projection.string_at("/message/role"),
        Some("system")
    );
    assert_eq!(
        persisted_projection.string_units("/message/content"),
        Some(expected_result_units.clone()),
        "persisted System.content retains Native's lone UTF-16 unit"
    );
    assert_eq!(
        persisted_entry.message, system_message,
        "the durable entry round-trips the accepted System row and source units"
    );
}

#[tokio::test]
async fn source_derived_user_lone_session_append_rewrite_preserves_return_and_transcript_units() {
    // Source: Native 2.1.295 append.test.ts
    // (SHA-256 3b9a34e4714a7b297f3cb0e6407397fd05bb67d81b7372670df019e5cf8fe1a6)
    // uses a user text of `"\ud800"`,
    // passes it to $.session.append, and asserts that the returned text is
    // `"prefix:" + input`. Native's hooks/register.js
    // (SHA-256 3304837a13c3232693e30ca2e9d5f71c3d86995d815df32d740c23a8d5c2ad84)
    // performs that rewrite. Native's public user event was not retained, so
    // this is a source-derived candidate input rather than a fabricated
    // captured-Native event. The candidate module maps Native's
    // `uuid === "rewrite"` selector to a stable UUID for the typed writer path.
    let input = Utf16JsonProjection::parse(
        r#"{"message":{"type":"user","content":[{"type":"text","text":"\ud800"}]},"door":"user","origin":{"kind":"core"},"uuid":"00000000-0000-4000-8000-000000000016"}"#,
    )
    .expect("source-derived user session.append input keeps the escaped lone unit");
    let input_units = input
        .string_units("/message/content/0/text")
        .expect("source user text units");
    assert_eq!(input_units, [0xd800_u16]);
    assert_eq!(input.string_at("/message/type"), Some("user"));
    assert_eq!(input.string_at("/uuid"), Some(USER_UTF16_UUID));

    let mut expected_result_units = "prefix:".encode_utf16().collect::<Vec<_>>();
    expected_result_units.extend_from_slice(&input_units);
    let fixture_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let module = fixture_root.join("current_alignment_mod_append_user_utf16_295.js");
    let host = ModHost::start(None).await.expect("start actual Mod worker");
    host.load(
        "current-alignment-user-utf16-session-append",
        &fixture_root,
        &module,
        Utf16JsonProjection::parse("{}").expect("empty Mod options JSON"),
    )
    .await
    .expect("load source-derived user rewrite into Bun");

    let next_inputs = Arc::new(Mutex::new(Vec::<Utf16JsonProjection>::new()));
    let captured = next_inputs.clone();
    let returned = host
        .dispatch("session.append", input, move |event| {
            let captured = captured.clone();
            async move {
                let event = event.expect("user append next carries an event");
                captured
                    .lock()
                    .expect("forwarded user event lock")
                    .push(event.clone());
                Ok(Some(
                    Utf16JsonProjection::object(vec![
                        (
                            "message".into(),
                            event.subprojection("/message").expect("message projection"),
                        ),
                        (
                            "uuid".into(),
                            event.subprojection("/uuid").expect("uuid projection"),
                        ),
                    ])
                    .expect("user append continuation result"),
                ))
            }
        })
        .await
        .expect("dispatch user append through Bun");
    assert_eq!(
        returned.string_units("/message/content/0/text"),
        Some(expected_result_units.clone()),
        "the public ModHost result returns prefix plus the exact lone UTF-16 unit"
    );
    assert_eq!(returned.string_at("/message/type"), Some("user"));
    assert_eq!(returned.string_at("/uuid"), Some(USER_UTF16_UUID));
    let forwarded = next_inputs
        .lock()
        .expect("forwarded user event lock")
        .first()
        .cloned()
        .expect("middleware rewrite reaches the real continuation");
    assert_eq!(
        forwarded.string_units("/message/content/0/text"),
        Some(expected_result_units.clone())
    );

    let registry = {
        let mut registry = hooks::HookRegistry::new();
        registry.set_mod_host(host.clone());
        registry
    };
    let executor = Arc::new(hooks::HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(test_harness::mocks::MockHttpTransport::new()),
        Arc::new(test_harness::mocks::MockRuntimeSpawner::default()),
    ));
    let temp = tempfile::tempdir().expect("isolated user transcript directory");
    let transcript_path = temp.path().join("user.jsonl");
    let writer = AgentTranscriptWriter::new(
        transcript_path.clone(),
        AgentId::new(),
        Arc::new(platform_posix::PosixFileSystem::new(
            temp.path().to_path_buf(),
        )),
    )
    .with_mod_append(
        Some(executor),
        temp.path().to_path_buf(),
        Some("test-model".into()),
    );
    let mut user_message = user_with_utf16_text(case_id(USER_UTF16_UUID), input_units);
    writer
        .record_retained(&mut user_message, &[])
        .await
        .expect("persist rewritten user row through AgentTranscriptWriter");
    let ConversationMessage::User { content, .. } = &user_message else {
        panic!("retained message remains a user row")
    };
    let ContentBlock::TextJsUtf16 {
        text,
        utf16_code_units,
        ..
    } = &content[0]
    else {
        panic!("rewritten user text retains its UTF-16 source carrier")
    };
    assert_eq!(text, "prefix:\u{fffd}");
    assert_eq!(utf16_code_units, &expected_result_units);

    let persisted = std::fs::read(&transcript_path).expect("read rewritten user JSONL");
    assert!(persisted
        .windows(b"prefix:\\ud800".len())
        .any(|window| window == b"prefix:\\ud800"));
    assert!(!persisted
        .windows(3)
        .any(|window| window == [0xef, 0xbf, 0xbd]));
    let persisted_line = std::str::from_utf8(&persisted)
        .expect("JSONL framing stays UTF-8 with the source unit escaped")
        .lines()
        .next()
        .expect("one persisted user row");
    let persisted_projection = Utf16JsonProjection::parse(persisted_line)
        .expect("persisted user row parses as exact UTF-16 JSON projection");
    assert_eq!(
        persisted_projection.string_at("/message/role"),
        Some("user")
    );
    assert_eq!(
        persisted_projection.string_at("/message/id"),
        Some(USER_UTF16_UUID)
    );
    assert_eq!(
        persisted_projection.string_units("/message/content/0/text"),
        Some(expected_result_units),
        "transcript source projection retains prefix plus the original lone unit"
    );
}
