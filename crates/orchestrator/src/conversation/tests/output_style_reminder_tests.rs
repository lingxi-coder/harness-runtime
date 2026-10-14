use super::*;
use crate::test_support::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    mock_message_response, noop_hook_executor, text_delta, MockApiClient, MockOutputStream,
    MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use lingxi_core::types::{utf16_json::Utf16JsonProjection as Source, ContentBlock};
use llm_runtime::ContentBlock as LlmContentBlock;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

fn source(value: &serde_json::Value) -> Source {
    Source::from_serde_value(value).expect("generated fixture JSON is valid")
}

/// The byte-exact reminder text for the `Explanatory` builtin — 1:1 with TS
/// `wrapInSystemReminder(`${name} output style is active. …`)`
/// (`messages.ts:3097-3099` + `3805-3810`).
const EXPLANATORY_REMINDER: &str = "<system-reminder>\nExplanatory output style is active. \
     Remember to follow the specific guidelines for this style.\n</system-reminder>";
const LEARNING_REMINDER: &str = "<system-reminder>\nLearning output style is active. \
     Remember to follow the specific guidelines for this style.\n</system-reminder>";

#[tokio::test]
async fn plan_attachment_precedes_output_style_in_outgoing_order() {
    let orch = ConversationOrchestrator::new(
        config_with_style("Explanatory"),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    {
        let session = orch.session();
        let mut state = session.lock().await;
        state.plan_mode = true;
        state.plan_reminder_shown = false;
    }
    let reminders = orch
        .collect_turn_reminders(true, true, None, &[])
        .await
        .expect("valid output-style fixture settings build reminders");
    let plan = reminders
        .transient
        .iter()
        .position(|message| message.text_content().contains("Plan mode is active."))
        .expect("plan reminder");
    let style = reminders
        .transient
        .iter()
        .position(|message| message.text_content() == EXPLANATORY_REMINDER)
        .expect("output style reminder");
    assert!(plan < style, "native attachment fan-out sends plan first");
}

#[tokio::test]
async fn live_settings_refresh_can_clear_the_boot_output_style() {
    let orch = ConversationOrchestrator::new(
        config_with_style("Explanatory"),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );

    assert_eq!(
        orch.active_output_style_setting()
            .await
            .and_then(|setting| setting.string_at("").map(str::to_owned)),
        Some("Explanatory".to_owned())
    );
    orch.refresh_native_output_style(None).await;
    assert!(
        orch.active_output_style_setting().await.is_none(),
        "a live clear is distinct from never having received a live refresh"
    );
    assert!(orch.resolve_active_output_style().await.is_none());
}

#[tokio::test]
async fn mod_attachment_rewrites_the_outgoing_style_reminder_and_caches_by_message_id() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("attachment.js");
    std::fs::write(
        &module,
        r#"let calls = 0;
           export function register(on) {
             on('prompt.attachment', { type: 'output_style' }, ($, e, next) =>
               e.text.startsWith('Learning')
                 ? { text: null }
                 : next({ ...e, text: `rewritten ${++calls}` }));
             on('prompt.submit', ($, e, next) => {
               $.ui.invalidate('prompt.attachment');
               return next(e);
             });
           }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load(
        "attachment",
        dir.path(),
        &module,
        Source::parse("{}").unwrap(),
    )
    .await
    .unwrap();

    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host.clone());
    let orch = ConversationOrchestrator::new(
        config_with_style("Explanatory"),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));

    let reminders = orch
        .collect_turn_reminders(true, true, None, &[])
        .await
        .expect("valid output-style fixture settings build reminders");
    assert!(reminders.transient.iter().any(|message| {
        text_of(message) == "<system-reminder>\nrewritten 1\n</system-reminder>"
    }));
    assert!(!reminders
        .transient
        .iter()
        .any(|message| text_of(message) == EXPLANATORY_REMINDER));

    let original = orch.output_style_reminder_message().await.unwrap();
    let first = orch
        .mod_prompt_attachment(
            "output_style",
            original.clone(),
            serde_json::json!({"kind":"engine"}),
        )
        .await
        .unwrap();
    let replay = orch
        .mod_prompt_attachment(
            "output_style",
            original,
            serde_json::json!({"kind":"engine"}),
        )
        .await
        .unwrap();
    assert_eq!(
        text_of(&first),
        "<system-reminder>\nrewritten 2\n</system-reminder>"
    );
    assert_eq!(text_of(&replay), text_of(&first));

    let another = orch.output_style_reminder_message().await.unwrap();
    let another = orch
        .mod_prompt_attachment(
            "output_style",
            another,
            serde_json::json!({"kind":"engine"}),
        )
        .await
        .unwrap();
    assert_eq!(
        text_of(&another),
        "<system-reminder>\nrewritten 3\n</system-reminder>"
    );

    host.dispatch_with_log_at_session(
        "prompt.submit",
        source(&serde_json::json!({"text":"refresh"})),
        &orch,
        |event| async move { Ok(event) },
        |_, _| async {},
    )
    .await
    .unwrap();
    let refreshed = orch.output_style_reminder_message().await.unwrap();
    let refreshed = orch
        .mod_prompt_attachment(
            "output_style",
            refreshed,
            serde_json::json!({"kind":"engine"}),
        )
        .await
        .unwrap();
    assert_eq!(
        text_of(&refreshed),
        "<system-reminder>\nrewritten 4\n</system-reminder>"
    );

    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let learning = ConversationOrchestrator::new(
        config_with_style("Learning"),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)));
    assert!(!learning
        .collect_turn_reminders(true, true, None, &[])
        .await
        .expect("valid Learning-style fixture settings build reminders")
        .transient
        .iter()
        .any(|message| text_of(message) == LEARNING_REMINDER));
}

/// Config with a non-default builtin output style active.
fn config_with_style(style: &str) -> OrchestratorConfig {
    OrchestratorConfig {
        output_style: Some(
            lingxi_core::types::utf16_json::Utf16JsonProjection::root_string(
                style.to_string(),
                style.encode_utf16().collect(),
            )
            .unwrap(),
        ),
        ..OrchestratorConfig::default()
    }
}

/// Concatenated text of a message's text blocks (for substring checks).
fn text_of(msg: &ConversationMessage) -> String {
    match msg {
        ConversationMessage::User { content, .. }
        | ConversationMessage::Assistant { content, .. } => content
            .iter()
            .filter_map(ContentBlock::visible_text)
            .collect::<Vec<_>>()
            .join(""),
        ConversationMessage::System { content, .. } => {
            content.string_at("").unwrap_or_default().to_owned()
        }
    }
}

fn is_reminder(msg: &ConversationMessage, expected: &str) -> bool {
    matches!(msg, ConversationMessage::User { .. }) && text_of(msg) == expected
}

fn attachment_positions(
    orch: &ConversationOrchestrator,
    messages: &[ConversationMessage],
    kind: &str,
) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            orch.context_attachment_history(std::slice::from_ref(message))
                .iter()
                .any(|attachment| attachment.string_at("/type") == Some(kind))
                .then_some(index)
        })
        .collect()
}

fn unique_attachment_position(
    orch: &ConversationOrchestrator,
    messages: &[ConversationMessage],
    kind: &str,
) -> usize {
    let positions = attachment_positions(orch, messages, kind);
    assert_eq!(positions.len(), 1, "expected one {kind} attachment");
    positions[0]
}

/// An empty ordinary user turn stages the common environment/model rows,
/// retains the session/date announcements, and stages the usual token row.
/// A selected style adds its distinct session-settings payload; listing and
/// budget rows are absent for this empty-tool, unbudgeted fixture.
fn assert_empty_regular_ingress_inventory(
    orch: &ConversationOrchestrator,
    messages: &[ConversationMessage],
    expected_style: Option<&str>,
) {
    let attachments = orch.context_attachment_history(messages);
    let expected_count = 5 + usize::from(expected_style.is_some());
    assert_eq!(attachments.len(), expected_count, "typed ingress inventory");
    let kinds = attachments
        .iter()
        .map(|attachment| {
            attachment
                .string_at("/type")
                .expect("typed context attachment")
        })
        .collect::<Vec<_>>();
    let mut expected = vec!["environment", "model"];
    if expected_style.is_some() {
        expected.push("output_style_instructions");
    }
    expected.extend(["total_tokens_reminder", "session_context", "date"]);
    assert_eq!(kinds, expected, "Native typed ingress order");
    if let Some(name) = expected_style {
        let style = attachments
            .iter()
            .find(|attachment| attachment.string_at("/type") == Some("output_style_instructions"))
            .expect("the selected style has its session-settings payload");
        assert_eq!(style.string_at("/style/name"), Some(name));
    }
}

/// Assert the complete request-local layout, including the transient output
/// style message which intentionally has no durable typed attachment payload.
fn assert_regular_request_layout(
    orch: &ConversationOrchestrator,
    messages: &[ConversationMessage],
    prompt: &str,
    style_reminder: Option<&str>,
) {
    let mut expected = vec!["environment".to_owned(), "model".to_owned()];
    if style_reminder.is_some() {
        expected.push("output_style_instructions".to_owned());
        expected.push("output_style".to_owned());
    }
    expected.extend(
        ["total_tokens_reminder", "session_context", "date"]
            .into_iter()
            .map(str::to_owned),
    );
    let actual = messages
        .iter()
        .filter(|message| text_of(message) != prompt)
        .map(|message| {
            let attachments = orch.context_attachment_history(std::slice::from_ref(message));
            if let Some(kind) = attachments
                .first()
                .and_then(|attachment| attachment.string_at("/type"))
            {
                kind.to_owned()
            } else if style_reminder.is_some_and(|style| is_reminder(message, style)) {
                "output_style".to_owned()
            } else {
                "<untyped>".to_owned()
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, expected, "Native request attachment order");
}

/// Empty eager files still announce a durable session-context anchor and date
/// after the human prompt; the empty anchor has no model projection.
fn assert_normal_empty_context(
    orch: &ConversationOrchestrator,
    sent: &[ConversationMessage],
    prompt: &str,
) {
    assert_eq!(
        sent.iter()
            .filter(|message| text_of(message) == prompt)
            .count(),
        1,
        "the original prompt appears once in the request"
    );
    let context_index = unique_attachment_position(orch, sent, "session_context");
    let date_index = unique_attachment_position(orch, sent, "date");
    assert_ne!(context_index, date_index);
    assert!(
        matches!(&sent[context_index], ConversationMessage::System { content, subtype, .. }
        if content.string_at("").is_some_and(str::is_empty)
            && subtype.as_deref() == Some("model_reminder_attachment"))
    );
    let date = crate::prompt::env_meta::current_date_string();
    assert!(
        orch.context_attachment_history(std::slice::from_ref(&sent[context_index]))[0]
            .semantically_eq(&source(
                &serde_json::json!({"type":"session_context", "context":{}})
            ))
    );
    assert!(
        orch.context_attachment_history(std::slice::from_ref(&sent[date_index]))[0]
            .semantically_eq(&source(&serde_json::json!({"type":"date", "date":date})))
    );
    assert_eq!(
        text_of(&sent[date_index]),
        format!("<system-reminder>\nToday's date is {date}.\n</system-reminder>")
    );
}

// ----- direct unit coverage of the reminder builder -----

#[tokio::test]
async fn builder_emits_byte_exact_explanatory_reminder() {
    let orch = ConversationOrchestrator::new(
        config_with_style("Explanatory"),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    let msg = orch
        .output_style_reminder_message()
        .await
        .expect("Explanatory resolves to a reminder");
    assert!(matches!(msg, ConversationMessage::User { .. }));
    assert_eq!(text_of(&msg), EXPLANATORY_REMINDER);
    // Spell out the literal bytes once so a drift in the helper const is caught.
    assert_eq!(
        text_of(&msg),
        "<system-reminder>\nExplanatory output style is active. Remember to follow the specific guidelines for this style.\n</system-reminder>"
    );
}

#[tokio::test]
async fn builder_emits_byte_exact_learning_reminder() {
    let orch = ConversationOrchestrator::new(
        config_with_style("Learning"),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    assert_eq!(
        text_of(
            &orch
                .output_style_reminder_message()
                .await
                .expect("Learning resolves")
        ),
        LEARNING_REMINDER
    );
}

#[tokio::test]
async fn builder_returns_none_for_default_and_unknown_styles() {
    for style in [None, Some("default"), Some(""), Some("Nonexistent")] {
        let cfg = OrchestratorConfig {
            output_style: style.map(|style| {
                Source::root_string(style.to_owned(), style.encode_utf16().collect()).unwrap()
            }),
            ..OrchestratorConfig::default()
        };
        let orch = ConversationOrchestrator::new(
            cfg,
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );
        assert!(
            orch.output_style_reminder_message().await.is_none(),
            "style {style:?} must not produce a reminder"
        );
    }
}

// ----- CLI-1: `/output-style` switches take effect on the NEXT turn ---------

fn orch_with(cfg: OrchestratorConfig) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        cfg,
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
}

/// `OrchestratorConfig` is a boot snapshot, so before this the active style was
/// fixed for the life of the session and `/output-style` could not have had any
/// effect no matter how it was wired.
#[tokio::test]
async fn a_runtime_switch_changes_the_style_the_next_turn_resolves() {
    use lingxi_core::host::OrchestratorHandle;
    let orch = orch_with(config_with_style("Explanatory"));
    assert_eq!(
        orch.resolve_active_output_style().await.map(|s| s.name),
        Some("Explanatory".to_string())
    );

    orch.set_output_style("Learning")
        .await
        .expect("Learning is a builtin");
    assert_eq!(
        orch.resolve_active_output_style().await.map(|s| s.name),
        Some("Learning".to_string()),
        "the switch must be visible to prompt assembly, not just recorded"
    );
}

/// Switching BACK to `default` has to beat the boot setting. Treating the live
/// value as "unset" whenever it names the default style would make the default
/// the one style a session could never return to.
#[tokio::test]
async fn switching_back_to_default_overrides_the_boot_setting() {
    use lingxi_core::host::OrchestratorHandle;
    let orch = orch_with(config_with_style("Explanatory"));
    orch.set_output_style("default")
        .await
        .expect("default is always selectable");
    assert!(
        orch.resolve_active_output_style().await.is_none(),
        "an explicit `default` must clear the inherited style"
    );
}

#[tokio::test]
async fn an_unknown_style_is_refused_rather_than_silently_ignored() {
    use lingxi_core::host::OrchestratorHandle;
    let orch = orch_with(config_with_style("Explanatory"));
    assert!(orch.set_output_style("Nonexistent").await.is_err());
    assert_eq!(
        orch.resolve_active_output_style().await.map(|s| s.name),
        Some("Explanatory".to_string()),
        "a refused switch must leave the previous style in force"
    );
}

/// The listing is what `/output-style` renders, so it has to contain every
/// selectable name — including `default`, which upstream lists with no
/// description because its style table maps that key to `null`.
#[tokio::test]
async fn the_listing_contains_default_plus_every_builtin() {
    use lingxi_core::host::OrchestratorHandle;
    let orch = orch_with(config_with_style("Learning"));
    let listing = orch.output_styles().await.expect("a listing");
    assert_eq!(listing.current, "Learning");
    let names: Vec<&str> = listing.styles.iter().map(|(n, _)| n.as_str()).collect();
    // Upstream table order, `default` first.
    assert_eq!(
        names,
        vec!["default", "Proactive", "Concise", "Explanatory", "Learning"]
    );
    assert!(
        listing.styles[0].1.is_none(),
        "`default` carries no description upstream"
    );
    assert!(listing.styles[1].1.is_some());
}

/// A style that carries its own `turnReminder` must render THAT sentence, not
/// the generic fallback. This arm was unreachable until `Proactive` and
/// `Concise` were ported, and the file used to argue at length that it always
/// would be.
#[tokio::test]
async fn a_style_with_its_own_turn_reminder_renders_it_instead_of_the_fallback() {
    for (style, sentence) in [
        (
            "Proactive",
            "Execute autonomously, minimize interruptions, prefer action over planning.",
        ),
        (
            "Concise",
            "Be concise: lead with the result, skip preamble and narration, keep only what the user needs.",
        ),
    ] {
        let orch = orch_with(config_with_style(style));
        let msg = orch
            .output_style_reminder_message()
            .await
            .expect("a non-default style reminds every turn");
        assert_eq!(
            text_of(&msg),
            format!(
                "<system-reminder>\n{style} output style is active. {sentence}\n</system-reminder>"
            )
        );
    }
}

/// …and a style that carries none keeps the generic sentence, so adding the
/// field did not quietly change what `Explanatory` / `Learning` send.
#[tokio::test]
async fn a_style_without_one_keeps_the_generic_reminder() {
    let orch = orch_with(config_with_style("Explanatory"));
    assert_eq!(
        text_of(
            &orch
                .output_style_reminder_message()
                .await
                .expect("reminder")
        ),
        EXPLANATORY_REMINDER
    );
}

/// The two new builtins have to be real styles, not just names in a listing:
/// selecting one must put its verbatim body into the system prompt.
#[tokio::test]
async fn the_new_builtins_resolve_to_their_verbatim_bodies() {
    for (style, opening, heading) in [
        (
            "Proactive",
            "You should work proactively and autonomously",
            "# Proactive Style Active",
        ),
        (
            "Concise",
            "Keep your responses short and direct",
            "# Concise Style Active",
        ),
    ] {
        let orch = orch_with(config_with_style(style));
        let resolved = orch
            .resolve_active_output_style()
            .await
            .unwrap_or_else(|| panic!("{style} must resolve"));
        assert_eq!(resolved.name, style);
        assert!(resolved.prompt.contains(opening), "{style}: preamble");
        assert!(resolved.prompt.contains(heading), "{style}: heading");
        assert!(
            resolved.keep_coding_instructions,
            "{style}: keepCodingInstructions is true for every builtin"
        );
    }
}

// ----- batched driver (`run_turn` / `execute_one_turn`) -----

#[tokio::test]
async fn batched_active_style_appends_transient_reminder_not_persisted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(dir.path().to_path_buf()),
    );
    let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
        session_path.clone(),
        fs,
    ));

    let resp = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "assistant body".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    let api = Arc::new(MockApiClient::new(vec![resp]));
    let orch = ConversationOrchestrator::new(
        config_with_style("Explanatory"),
        api.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_jsonl_writer(writer);

    orch.run_turn("user prompt body").await.expect("turn");

    // The selected style adds a durable session-settings row; the ordinary
    // output-style reminder remains a separate transient row.
    let outgoing = api.captured_msgs().await;
    assert_eq!(outgoing.len(), 1, "exactly one batched API call");
    let sent = &outgoing[0];
    assert_eq!(
        sent.len(),
        8,
        "prompt + six typed attachments + transient style reminder"
    );
    assert_empty_regular_ingress_inventory(&orch, sent, Some("Explanatory"));
    assert_regular_request_layout(&orch, sent, "user prompt body", Some(EXPLANATORY_REMINDER));
    assert_normal_empty_context(&orch, sent, "user prompt body");
    assert_eq!(
        sent.iter()
            .filter(|message| is_reminder(message, EXPLANATORY_REMINDER))
            .count(),
        1,
        "one transient output-style reminder"
    );
    let total_tokens_index = unique_attachment_position(&orch, sent, "total_tokens_reminder");
    assert!(text_of(&sent[total_tokens_index]).contains("<total_tokens>"));
    let history = orch.session.lock().await.history.clone();
    assert_eq!(
        history.len(),
        8,
        "human + environment/model/style ingress + context + date + total_tokens + assistant"
    );
    assert!(history
        .iter()
        .all(|message| !is_reminder(message, EXPLANATORY_REMINDER)));
    assert_eq!(text_of(&history[0]), "user prompt body");
    for kind in [
        "environment",
        "model",
        "output_style_instructions",
        "session_context",
        "date",
        "total_tokens_reminder",
    ] {
        let outgoing_index = unique_attachment_position(&orch, sent, kind);
        let message_id = sent[outgoing_index].id();
        assert_eq!(
            history
                .iter()
                .filter(|message| message.id() == message_id)
                .count(),
            1,
            "the durable {kind} row retains its outgoing identity"
        );
    }
    assert_eq!(
        text_of(history.last().expect("assistant reply")),
        "assistant body"
    );

    // JSONL retains the ordinary durable attachments, with style reminder text absent.
    let on_disk = std::fs::read_to_string(&session_path).expect("read jsonl");
    assert!(on_disk.contains("user prompt body"));
    assert!(on_disk.contains("assistant body"));
    assert!(
        !on_disk.contains("output style is active"),
        "the reminder must never be persisted to JSONL; file:\n{on_disk}"
    );
}

#[tokio::test]
async fn batched_default_style_sends_no_reminder() {
    let resp = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "body".into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    );
    let api = Arc::new(MockApiClient::new(vec![resp]));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(), // output_style: None
        api.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );

    orch.run_turn("just the prompt").await.expect("turn");

    let outgoing = api.captured_msgs().await;
    assert_eq!(outgoing.len(), 1);
    assert_eq!(outgoing[0].len(), 6, "prompt + five typed attachments");
    assert_empty_regular_ingress_inventory(&orch, &outgoing[0], None);
    assert_regular_request_layout(&orch, &outgoing[0], "just the prompt", None);
    assert_normal_empty_context(&orch, &outgoing[0], "just the prompt");
    let total_tokens_index =
        unique_attachment_position(&orch, &outgoing[0], "total_tokens_reminder");
    assert_eq!(
        orch.context_attachment_history(std::slice::from_ref(&outgoing[0][total_tokens_index]))[0]
            .string_at("/type"),
        Some("total_tokens_reminder")
    );
    assert!(text_of(&outgoing[0][total_tokens_index]).contains("<total_tokens>"));
    assert!(
        !outgoing[0]
            .iter()
            .any(|m| is_reminder(m, EXPLANATORY_REMINDER) || is_reminder(m, LEARNING_REMINDER)),
        "no output-style reminder on the default path; got {:?}",
        outgoing[0]
    );
}

// ----- streaming driver (`run_turn_streaming`) -----

#[tokio::test]
async fn streaming_active_style_appends_transient_reminder_not_persisted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(dir.path().to_path_buf()),
    );
    let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
        session_path.clone(),
        fs,
    ));

    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start("m1", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "streamed body"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            config_with_style("Learning"),
            Arc::new(MockApiClient::new(vec![])),
            streaming.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_jsonl_writer(writer),
    );

    orch.run_turn_streaming("streaming prompt")
        .await
        .expect("streaming turn");

    let calls = streaming.captured_calls().await;
    assert_eq!(calls.len(), 1, "exactly one streaming call");
    let sent = &calls[0].messages;
    assert_eq!(
        sent.len(),
        8,
        "prompt + six typed attachments + transient style reminder"
    );
    assert_empty_regular_ingress_inventory(&orch, sent, Some("Learning"));
    assert_regular_request_layout(&orch, sent, "streaming prompt", Some(LEARNING_REMINDER));
    assert_normal_empty_context(&orch, sent, "streaming prompt");
    assert_eq!(
        sent.iter()
            .filter(|message| is_reminder(message, LEARNING_REMINDER))
            .count(),
        1,
        "one transient output-style reminder"
    );
    let total_tokens_index = unique_attachment_position(&orch, sent, "total_tokens_reminder");
    assert!(text_of(&sent[total_tokens_index]).contains("<total_tokens>"));

    // STORED history: reminder absent.
    let history = orch.session.lock().await.history.clone();
    assert!(
        history.iter().all(|m| !is_reminder(m, LEARNING_REMINDER)),
        "the reminder must never enter stored history; got {history:?}"
    );
    assert_eq!(
        history.len(),
        8,
        "human + environment/model/style ingress + context + date + total_tokens + assistant"
    );
    assert_eq!(text_of(&history[0]), "streaming prompt");
    for kind in [
        "environment",
        "model",
        "output_style_instructions",
        "session_context",
        "date",
        "total_tokens_reminder",
    ] {
        let outgoing_index = unique_attachment_position(&orch, sent, kind);
        let message_id = sent[outgoing_index].id();
        assert_eq!(
            history
                .iter()
                .filter(|message| message.id() == message_id)
                .count(),
            1,
            "the durable {kind} row retains its outgoing identity"
        );
    }
    assert_eq!(
        text_of(history.last().expect("assistant reply")),
        "streamed body"
    );

    // JSONL transcript: reminder text absent.
    let on_disk = std::fs::read_to_string(&session_path).expect("read jsonl");
    assert!(on_disk.contains("streaming prompt"));
    assert!(
        !on_disk.contains("output style is active"),
        "the reminder must never be persisted to JSONL; file:\n{on_disk}"
    );
}

#[tokio::test]
async fn streaming_default_style_sends_no_reminder() {
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start("m1", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "body"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(), // output_style: None
        Arc::new(MockApiClient::new(vec![])),
        streaming.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));

    orch.run_turn_streaming("only prompt")
        .await
        .expect("streaming turn");

    let calls = streaming.captured_calls().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].messages.len(),
        6,
        "prompt + five typed attachments"
    );
    assert_empty_regular_ingress_inventory(&orch, &calls[0].messages, None);
    assert_regular_request_layout(&orch, &calls[0].messages, "only prompt", None);
    assert_normal_empty_context(&orch, &calls[0].messages, "only prompt");
    let total_tokens_index =
        unique_attachment_position(&orch, &calls[0].messages, "total_tokens_reminder");
    assert_eq!(
        orch.context_attachment_history(std::slice::from_ref(
            &calls[0].messages[total_tokens_index],
        ))[0]
            .string_at("/type"),
        Some("total_tokens_reminder")
    );
    assert!(text_of(&calls[0].messages[total_tokens_index]).contains("<total_tokens>"));
}
