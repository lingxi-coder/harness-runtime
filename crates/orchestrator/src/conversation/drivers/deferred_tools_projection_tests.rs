use super::*;
use crate::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};
use llm_runtime::ContentBlock as LlmContentBlock;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;
use uuid::Uuid;

fn enabled_projection_facts(
    inline_tool_definition_names: &[&str],
) -> crate::request_context::DeferredToolsProjectionFacts {
    crate::request_context::DeferredToolsProjectionFacts {
        surface_late_tool_additions: true,
        inline_tool_definition_names: Some(
            inline_tool_definition_names
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
        ),
        deferred_wire_tool_names: Vec::new(),
    }
}

fn deferred_wire_projection_facts(
    deferred_wire_tool_names: &[&str],
) -> crate::request_context::DeferredToolsProjectionFacts {
    crate::request_context::DeferredToolsProjectionFacts {
        surface_late_tool_additions: true,
        inline_tool_definition_names: None,
        deferred_wire_tool_names: deferred_wire_tool_names
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
    }
}

fn empty_deferred_placeholder(id: MessageId) -> ConversationMessage {
    let mut placeholder = ConversationMessage::user_meta(id, String::new());
    if let ConversationMessage::User { content, .. } = &mut placeholder {
        content.clear();
    }
    placeholder
}

fn jsonl_user_row(session_id: Uuid, id: Uuid, text: &str) -> session::jsonl::JsonlMessage {
    let content = serde_json::to_string(&serde_json::json!([
        {"type": "text", "text": text}
    ]))
    .expect("serialized user content");
    serde_json::from_str(&format!(
        "{{\"type\":\"user\",\"uuid\":\"{id}\",\"parentUuid\":null,\
         \"isSidechain\":false,\"timestamp\":\"2026-10-15T12:00:00.000Z\",\
         \"cwd\":\"/fixture\",\"version\":\"test\",\"sessionId\":\"{session_id}\",\
         \"message\":{{\"id\":\"{id}\",\"role\":\"user\",\"content\":{content}}}}}"
    ))
    .expect("valid cold user row")
}

fn jsonl_deferred_row(
    session_id: Uuid,
    id: Uuid,
    source: &Source,
    rendered: serde_json::Value,
) -> session::jsonl::JsonlMessage {
    let raw_attachment = source
        .to_json_string()
        .expect("serialized deferred sidecar");
    serde_json::from_str(&format!(
        "{{\"type\":\"attachment\",\"uuid\":\"{id}\",\"parentUuid\":null,\
         \"isSidechain\":false,\"timestamp\":\"2026-10-15T12:00:01.000Z\",\
         \"cwd\":\"/fixture\",\"version\":\"test\",\"sessionId\":\"{session_id}\",\
         \"message\":null,\"attachment\":{raw_attachment},\"renderedRole\":\"system\",\
         \"rendered\":{rendered}}}"
    ))
    .expect("valid cold deferred attachment row")
}

async fn prepared_snapshot(orchestrator: &ConversationOrchestrator) -> Vec<ConversationMessage> {
    orchestrator
        .prepare_turn_step(ModelCallPath::Batched, None, false, false, None)
        .await
        .expect("prepare model request")
        .snapshot
}

fn message_position(messages: &[ConversationMessage], id: MessageId) -> usize {
    messages
        .iter()
        .position(|message| message.id() == id)
        .expect("row remains in the message sequence")
}

fn projection_orchestrator(api: Arc<MockApiClient>) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
}

fn reminder_text(message: &ConversationMessage) -> String {
    match message {
        ConversationMessage::User { content, .. } => content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text, .. } => Some(text.clone()),
                ContentBlock::TextJsUtf16 {
                    utf16_code_units, ..
                } => Some(String::from_utf16_lossy(utf16_code_units)),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

#[test]
fn prepare_snapshot_reprojects_a_durable_sidecar_without_changing_its_id() {
    let id = MessageId::new();
    let source = Source::parse(
        r#"{"type":"deferred_tools_delta","surfacedNames":["Read","TaskCreate"],"addedNames":[],"addedLines":[]}"#,
    )
    .expect("valid deferred-tools source");
    let cached = ConversationMessage::user_meta(
        id,
        "<system-reminder>\nThe following tools just became available and are ready to use:\nRead\n</system-reminder>".into(),
    );
    let api = Arc::new(MockApiClient::new(Vec::new()));
    let orchestrator = projection_orchestrator(api);
    orchestrator
        .transcript
        .model_reminder_attachments
        .lock()
        .unwrap()
        .insert(id, source);
    let mut request = vec![cached];

    orchestrator.project_deferred_tools_request_view(
        &mut request,
        enabled_projection_facts(&["TaskCreate"]),
    );

    assert_eq!(request.len(), 1);
    assert_eq!(request[0].id(), id);
    assert!(reminder_text(&request[0]).contains("TaskCreate"));
    assert!(!reminder_text(&request[0]).contains("Read"));
}

#[tokio::test]
async fn retry_reattach_reprojects_the_raw_sidecar_without_adding_a_duplicate() {
    let id = MessageId::new();
    let source = Source::parse(
        r#"{"type":"deferred_tools_delta","surfacedNames":["Read","TaskCreate"],"addedNames":[],"addedLines":[]}"#,
    )
    .expect("valid deferred-tools source");
    let cached = ConversationMessage::user_meta(
        id,
        "<system-reminder>\nThe following tools just became available and are ready to use:\nRead\n</system-reminder>".into(),
    );
    let api = Arc::new(MockApiClient::new(Vec::new()));
    let orchestrator = projection_orchestrator(api);
    orchestrator
        .transcript
        .model_reminder_attachments
        .lock()
        .unwrap()
        .insert(id, source);
    let mut request = vec![cached.clone()];
    let mut turn_reminders = Vec::new();
    let mut guarded = Vec::new();
    let context = crate::conversation::PreparedContextAnnouncements::default();

    crate::request_context::scope_if_missing(&orchestrator, async {
        crate::request_context::set_deferred_tools_projection_facts(enabled_projection_facts(&[
            "TaskCreate",
        ]));
        orchestrator
            .reattach_outgoing_context(
                &mut request,
                Some(&cached),
                None,
                &mut turn_reminders,
                &mut guarded,
                &context,
                false,
            )
            .await;
        Ok(())
    })
    .await
    .expect("retry reattachment completes");

    assert_eq!(
        request.iter().filter(|message| message.id() == id).count(),
        1
    );
    let projected = request
        .iter()
        .find(|message| message.id() == id)
        .expect("durable deferred row remains");
    assert!(reminder_text(projected).contains("TaskCreate"));
    assert!(!reminder_text(projected).contains("Read"));
}

#[test]
fn cold_jsonl_restore_reprojects_the_raw_sidecar_with_the_same_id() {
    let session_id = Uuid::new_v4();
    let id = Uuid::new_v4();
    let source = Source::parse(
        r#"{"type":"deferred_tools_delta","surfacedNames":["Read","TaskCreate"],"addedNames":[],"addedLines":[]}"#,
    )
    .expect("valid deferred-tools source");
    let rendered = "<system-reminder>\nThe following tools just became available and are ready to use:\nRead\n</system-reminder>";
    let raw_attachment = source.to_json_string().expect("serialized source sidecar");
    let raw_rendered = serde_json::to_string(rendered).expect("serialized rendered cache");
    let row: session::jsonl::JsonlMessage = serde_json::from_str(&format!(
        "{{\"type\":\"attachment\",\"uuid\":\"{id}\",\"parentUuid\":null,\
         \"sessionId\":\"{session_id}\",\"timestamp\":\"2026-10-15T12:00:00.000Z\",\
         \"cwd\":\"/fixture\",\"version\":\"test\",\"isSidechain\":false,\
         \"message\":null,\"attachment\":{raw_attachment},\
         \"renderedRole\":\"system\",\"rendered\":[{{\"content\":{raw_rendered}}}]}}"
    ))
    .expect("valid cold attachment row");
    let cold_state = crate::resume::state_from_messages(session_id, std::slice::from_ref(&row));
    let mut request = cold_state.model_context_history();
    let cold_sidecars = crate::resume::model_reminder_attachments_from_messages(&[row]);
    let api = Arc::new(MockApiClient::new(Vec::new()));
    let orchestrator = projection_orchestrator(api);
    orchestrator
        .transcript
        .model_reminder_attachments
        .lock()
        .unwrap()
        .extend(cold_sidecars);

    orchestrator.project_deferred_tools_request_view(
        &mut request,
        enabled_projection_facts(&["TaskCreate"]),
    );

    let id = MessageId::from_uuid(id);
    assert_eq!(request.len(), 1);
    assert_eq!(request[0].id(), id);
    assert!(reminder_text(&request[0]).contains("TaskCreate"));
    assert!(!reminder_text(&request[0]).contains("Read"));
}

#[tokio::test]
async fn provider_request_omits_an_empty_deferred_placeholder() {
    let id = MessageId::new();
    let source = Source::parse(
        r#"{"type":"deferred_tools_delta","surfacedNames":["Read"],"addedNames":[],"addedLines":[]}"#,
    )
    .expect("valid deferred-tools source");
    let cached = empty_deferred_placeholder(id);
    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "done".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )]));
    let orchestrator = projection_orchestrator(Arc::clone(&api));
    {
        let mut session = orchestrator.session.lock().await;
        session.history.push(cached);
    }
    orchestrator
        .transcript
        .model_reminder_attachments
        .lock()
        .unwrap()
        .insert(id, source);

    let _ = orchestrator
        .run_turn_with_request_context(
            "send one request",
            Vec::new(),
            None,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

    let calls = api.captured_msgs().await;
    assert_eq!(calls.len(), 1, "the request reached the API boundary");
    assert!(
        calls[0].iter().all(|message| message.id() != id),
        "the empty current NQe projection must not put the cached row on the provider request"
    );
    assert!(
        calls[0]
            .iter()
            .all(|message| !reminder_text(message).contains("Read")),
        "the empty placeholder is internal and never sent as a provider user message"
    );
}

#[tokio::test]
async fn accepted_empty_deferred_row_keeps_hot_history_ordinal_and_native_group_order() {
    let before_id = MessageId::new();
    let after_id = MessageId::new();
    let id = MessageId::new();
    let payload = Source::parse(
        r#"{"type":"deferred_tools_delta","retractedTools":[{"name":"mcp__srv__tool","cause":"denied"}]}"#,
    )
    .expect("valid retracted deferred sidecar");
    let row = crate::request_context::PromptIngressAttachment {
        message: ConversationMessage::user_meta(id, String::new()),
        payload,
        rendered: Source::parse("[]").expect("valid empty rendered cache"),
    };
    let api = Arc::new(MockApiClient::new(Vec::new()));
    let orchestrator = projection_orchestrator(api);
    orchestrator
        .session
        .lock()
        .await
        .history
        .push(ConversationMessage::user(before_id, "before marker".into()));
    orchestrator
        .append_accepted_prompt_ingress_reminder(&row)
        .await;
    orchestrator
        .session
        .lock()
        .await
        .history
        .push(ConversationMessage::user(after_id, "after marker".into()));

    let history = orchestrator.session.lock().await.model_context_history();
    let placeholder = history
        .iter()
        .find(|message| message.id() == id)
        .expect("accepted empty-cache row remains in hot history");
    assert_eq!(
        crate::request_context::cached_rendered_row_count(placeholder),
        0,
        "the internal placeholder has no text block and is not a rendered cache row"
    );
    assert!(message_position(&history, before_id) < message_position(&history, id));
    assert!(message_position(&history, id) < message_position(&history, after_id));

    let snapshot = prepared_snapshot(&orchestrator).await;
    // Native zjr buffers typed attachments across ordinary user rows and
    // flushes only at assistant/tool-result/task-delivery boundaries. These
    // three rows share one boundary group, so the provider view moves the
    // attachment to the start while durable history keeps its source ordinal.
    assert!(message_position(&snapshot, id) < message_position(&snapshot, before_id));
    assert!(message_position(&snapshot, before_id) < message_position(&snapshot, after_id));
    let durable_history = orchestrator.session.lock().await.model_context_history();
    assert!(message_position(&durable_history, before_id) < message_position(&durable_history, id));
    assert!(message_position(&durable_history, id) < message_position(&durable_history, after_id));
    let projected = snapshot
        .iter()
        .find(|message| message.id() == id)
        .expect("deferred retraction has the original identity");
    assert!(reminder_text(projected).contains("Definitions of the following tools"));
}

#[tokio::test]
async fn cold_empty_render_cache_keeps_raw_history_ordinal_and_native_group_order() {
    let session_id = Uuid::new_v4();
    let uuid = Uuid::new_v4();
    let id = MessageId::from_uuid(uuid);
    let before_uuid = Uuid::new_v4();
    let after_uuid = Uuid::new_v4();
    let before_id = MessageId::from_uuid(before_uuid);
    let after_id = MessageId::from_uuid(after_uuid);
    let source = Source::parse(
        r#"{"type":"deferred_tools_delta","retractedTools":[{"name":"mcp__srv__tool","cause":"denied"}]}"#,
    )
    .expect("valid retracted deferred sidecar");
    let rows = vec![
        jsonl_user_row(session_id, before_uuid, "before marker"),
        jsonl_deferred_row(session_id, uuid, &source, serde_json::json!([])),
        jsonl_user_row(session_id, after_uuid, "after marker"),
    ];
    let cold_state = crate::resume::state_from_messages(session_id, &rows);
    let cold_history = cold_state.model_context_history();
    assert_eq!(cold_history.len(), 3, "cold restore retains the empty row");
    assert_eq!(cold_history[0].id(), before_id);
    assert_eq!(cold_history[1].id(), id);
    assert_eq!(cold_history[2].id(), after_id);
    assert_eq!(
        crate::request_context::cached_rendered_row_count(&cold_history[1]),
        0,
        "rendered:[] restores an identity placeholder without a text block"
    );

    let api = Arc::new(MockApiClient::new(Vec::new()));
    let orchestrator = projection_orchestrator(api);
    orchestrator.session.lock().await.history = cold_history;
    orchestrator
        .transcript
        .model_reminder_attachments
        .lock()
        .unwrap()
        .extend(crate::resume::model_reminder_attachments_from_messages(
            &rows,
        ));

    let snapshot = prepared_snapshot(&orchestrator).await;
    assert!(message_position(&snapshot, id) < message_position(&snapshot, before_id));
    assert!(message_position(&snapshot, before_id) < message_position(&snapshot, after_id));
    let durable_history = orchestrator.session.lock().await.model_context_history();
    assert!(message_position(&durable_history, before_id) < message_position(&durable_history, id));
    assert!(message_position(&durable_history, id) < message_position(&durable_history, after_id));
    let projected = snapshot
        .iter()
        .find(|message| message.id() == id)
        .expect("cold deferred retraction preserves its source identity");
    assert!(reminder_text(projected).contains("Definitions of the following tools"));
}

#[tokio::test]
async fn cold_valid_empty_string_cache_is_not_an_internal_placeholder() {
    let session_id = Uuid::new_v4();
    let uuid = Uuid::new_v4();
    let id = MessageId::from_uuid(uuid);
    let source = Source::parse(
        r#"{"type":"deferred_tools_delta","surfacedNames":[],"replacedNames":[],"addedNames":[],"addedLines":[]}"#,
    )
    .expect("valid no-op deferred source");
    let row = jsonl_deferred_row(
        session_id,
        uuid,
        &source,
        serde_json::json!([{"content": ""}]),
    );
    let cold_state = crate::resume::state_from_messages(session_id, std::slice::from_ref(&row));
    let cold_history = cold_state.model_context_history();
    assert_eq!(cold_history.len(), 1);
    assert_eq!(cold_history[0].id(), id);
    assert_eq!(
        crate::request_context::cached_rendered_row_count(&cold_history[0]),
        1,
        "a valid Native rendered row with empty content remains a cache row"
    );
    let ConversationMessage::User { content, .. } = &cold_history[0] else {
        panic!("cold rendered cache hydrates to a user message");
    };
    assert!(matches!(
        content.as_slice(),
        [ContentBlock::TextJsUtf16 {
            utf16_code_units,
            ..
        }] if utf16_code_units.is_empty()
    ));

    let api = Arc::new(MockApiClient::new(Vec::new()));
    let orchestrator = projection_orchestrator(api);
    orchestrator.session.lock().await.history = cold_history;
    orchestrator
        .transcript
        .model_reminder_attachments
        .lock()
        .unwrap()
        .extend(crate::resume::model_reminder_attachments_from_messages(&[
            row,
        ]));

    let snapshot = prepared_snapshot(&orchestrator).await;
    let projected = snapshot
        .iter()
        .find(|message| message.id() == id)
        .expect("valid empty-string cache survives request preparation");
    assert_eq!(
        crate::request_context::cached_rendered_row_count(projected),
        1
    );
    assert!(reminder_text(projected).is_empty());
}

#[test]
fn projection_uses_deferred_wire_names_without_inline_definitions() {
    let id = MessageId::new();
    let source = Source::parse(
        r#"{"type":"deferred_tools_delta","surfacedNames":["Read"],"addedNames":[],"addedLines":[]}"#,
    )
    .expect("valid deferred source");

    let projected = crate::request_context::render_deferred_tools_delta_for_request(
        id,
        &source,
        None,
        0,
        deferred_wire_projection_facts(&["Read"]),
        false,
    )
    .expect("deferred wire names can surface without by-value definitions");

    assert_eq!(projected.id(), id);
    assert!(reminder_text(&projected).contains("Read"));
}

#[test]
fn qwr_always_compares_raw_surfaced_and_replaced_source_arrays() {
    let id = MessageId::new();
    let gate_off = crate::request_context::DeferredToolsProjectionFacts::default();
    let cached_surfaced = ConversationMessage::user_meta(
        id,
        "<system-reminder>\nThe following tools just became available and are ready to use:\nRead\n</system-reminder>".into(),
    );
    let no_source_names = Source::parse(
        r#"{"type":"deferred_tools_delta","surfacedNames":[],"replacedNames":[],"addedNames":[],"addedLines":[]}"#,
    )
    .expect("valid empty name arrays");
    let matching_empty_set = crate::request_context::render_deferred_tools_delta_for_request(
        id,
        &no_source_names,
        Some(&cached_surfaced),
        1,
        gate_off.clone(),
        false,
    )
    .expect("qWr compares empty source names to Native's empty xr set");
    assert_eq!(
        reminder_text(&matching_empty_set),
        reminder_text(&cached_surfaced)
    );

    let surfaced_source = Source::parse(
        r#"{"type":"deferred_tools_delta","surfacedNames":["Read"],"replacedNames":[],"addedNames":[],"addedLines":[]}"#,
    )
    .expect("valid surfaced source name");
    assert!(
        crate::request_context::render_deferred_tools_delta_for_request(
            id,
            &surfaced_source,
            Some(&cached_surfaced),
            1,
            gate_off.clone(),
            false,
        )
        .is_none(),
        "raw surfacedNames differ from xr=[] even when surface additions are gated off"
    );

    let replaced_source = Source::parse(
        r#"{"type":"deferred_tools_delta","surfacedNames":[],"replacedNames":["Read"],"addedNames":[],"addedLines":[]}"#,
    )
    .expect("valid replaced source name");
    let cached_replaced = ConversationMessage::user_meta(
        id,
        "<system-reminder>\nThe following tools have updated definitions, which replace the earlier ones from here on:\nRead\n</system-reminder>".into(),
    );
    assert!(
        crate::request_context::render_deferred_tools_delta_for_request(
            id,
            &replaced_source,
            Some(&cached_replaced),
            1,
            gate_off,
            false,
        )
        .is_none(),
        "qWr includes replacedNames in raw sMn even without an eligible surfaced set"
    );
}
