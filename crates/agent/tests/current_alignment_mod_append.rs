//! Focused Native 2.1.293 session.append contract gate.
//!
//! Uses the same checked-in Mod module as the Native plugin-test fixture. The
//! first pass observes actual ModHost dispatch results and core `next` inputs;
//! the second pass exercises AgentTranscriptWriter's retained-row consumer so
//! a valid worker intermediate answer is not confused with persisted behavior.

use agent::transcript::AgentTranscriptWriter;
use hooks::mods::ModHost;
use lingxi_core::types::{
    utf16_json::Utf16JsonProjection, AgentId, ConversationMessage, MessageId,
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

fn case_id(uuid: &str) -> MessageId {
    MessageId::from_uuid(Uuid::parse_str(uuid).expect("fixed case UUID"))
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
    let native_event_path =
        fixture_root.join("native-baseline/system-content-lone-surrogate/result-event.raw.json");
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
    assert_eq!(
        persisted_projection.string_at("/message/type"),
        Some("system")
    );
    assert_eq!(
        persisted_projection.string_units("/message/content"),
        Some(expected_result_units),
        "persisted System.content retains Native's lone UTF-16 unit"
    );
}
