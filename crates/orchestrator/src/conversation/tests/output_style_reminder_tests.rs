use super::*;
use crate::test_support::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    mock_message_response, noop_hook_executor, text_delta, MockApiClient, MockOutputStream,
    MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use llm_runtime::ContentBlock as LlmContentBlock;
use protocol::ContentBlock;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

/// The byte-exact reminder text for the `Explanatory` builtin — 1:1 with TS
/// `wrapInSystemReminder(`${name} output style is active. …`)`
/// (`messages.ts:3097-3099` + `3805-3810`).
const EXPLANATORY_REMINDER: &str = "<system-reminder>\nExplanatory output style is active. \
     Remember to follow the specific guidelines for this style.\n</system-reminder>";
const LEARNING_REMINDER: &str = "<system-reminder>\nLearning output style is active. \
     Remember to follow the specific guidelines for this style.\n</system-reminder>";

/// Config with a non-default builtin output style active.
fn config_with_style(style: &str) -> OrchestratorConfig {
    OrchestratorConfig {
        output_style: Some(style.to_string()),
        ..OrchestratorConfig::default()
    }
}

/// Concatenated text of a message's text blocks (for substring checks).
fn text_of(msg: &ConversationMessage) -> String {
    match msg {
        ConversationMessage::User { content, .. }
        | ConversationMessage::Assistant { content, .. } => content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        ConversationMessage::System { content, .. } => content.clone(),
    }
}

fn is_reminder(msg: &ConversationMessage, expected: &str) -> bool {
    matches!(msg, ConversationMessage::User { .. }) && text_of(msg) == expected
}

/// True when `msg` is the leading `additionalContext` (`# claudeMd` /
/// `# userEmail` / `# currentDate`) meta message prepended each turn
/// (R-P1c/R-P1d). With `StaticMemoryProvider::empty()` and no `user_email`
/// it carries only the always-present `# currentDate` entry.
fn is_additional_context(msg: &ConversationMessage) -> bool {
    matches!(msg, ConversationMessage::User { .. })
        && text_of(msg).starts_with(
            "<system-reminder>\nAs you answer the user's questions, you can use the following context:",
        )
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
            output_style: style.map(str::to_string),
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
    use platform_api::OrchestratorHandle;
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
    use platform_api::OrchestratorHandle;
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
    use platform_api::OrchestratorHandle;
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
    use platform_api::OrchestratorHandle;
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
            format!("<system-reminder>\n{style} output style is active. {sentence}\n</system-reminder>")
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
    let fs: Arc<dyn platform_api::FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
        dir.path().to_path_buf(),
    ));
    let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
        session_path.clone(),
        fs,
    ));

    let resp = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "assistant body".into(),
            cache_control: None,
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

    // OUTGOING snapshot: [additionalContext(meta), user(prompt), reminder,
    // total_tokens reminder] —
    // the leading additional-context meta message (R-P1c/d) prepends the
    // user prompt; the output-style reminder trails it (TS position).
    let outgoing = api.captured_msgs().await;
    assert_eq!(outgoing.len(), 1, "exactly one batched API call");
    let sent = &outgoing[0];
    assert_eq!(
        sent.len(),
        4,
        "additionalContext + prompt + reminder + total_tokens; got {sent:?}"
    );
    assert!(
        is_additional_context(&sent[0]),
        "leading meta; got {:?}",
        sent[0]
    );
    assert_eq!(text_of(&sent[1]), "user prompt body");
    assert!(
        is_reminder(&sent[2], EXPLANATORY_REMINDER),
        "style message must be the byte-exact reminder; got {:?}",
        sent[2]
    );
    assert!(
        text_of(&sent[3]).contains("<total_tokens>"),
        "total-tokens reminder trails the batch; got {:?}",
        sent[3]
    );

    // STORED history: [user(prompt), assistant] — the reminder was NOT pushed.
    let history = orch.session.lock().await.history.clone();
    assert_eq!(history.len(), 2, "user + assistant only; got {history:?}");
    assert!(
        history
            .iter()
            .all(|m| !is_reminder(m, EXPLANATORY_REMINDER)),
        "the reminder must never enter stored history; got {history:?}"
    );
    assert_eq!(text_of(&history[0]), "user prompt body");
    assert_eq!(text_of(&history[1]), "assistant body");

    // JSONL transcript: user + assistant only, reminder text absent.
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
    // No output-style reminder; additional context leads and the stock
    // total-tokens reminder trails the prompt.
    assert_eq!(
        outgoing[0].len(),
        3,
        "additionalContext + prompt + total_tokens; got {:?}",
        outgoing[0]
    );
    assert!(
        is_additional_context(&outgoing[0][0]),
        "leading meta; got {:?}",
        outgoing[0][0]
    );
    assert_eq!(text_of(&outgoing[0][1]), "just the prompt");
    assert!(text_of(&outgoing[0][2]).contains("<total_tokens>"));
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
    let fs: Arc<dyn platform_api::FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
        dir.path().to_path_buf(),
    ));
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
    let orch = ConversationOrchestrator::new_with_streaming(
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
    .with_jsonl_writer(writer);

    orch.run_turn_streaming("streaming prompt")
        .await
        .expect("streaming turn");

    // OUTGOING snapshot to the stream: [additionalContext(meta), user(prompt),
    // output-style reminder, total_tokens reminder]. The total-tokens block
    // comes LAST, matching the oracle's fan-out order
    // (`…critical_system_reminder, silent_turn_reminder,
    // total_tokens_reminder`), and it is present because that reminder
    // defaults ON as it does upstream.
    let calls = streaming.captured_calls().await;
    assert_eq!(calls.len(), 1, "exactly one streaming call");
    let sent = &calls[0].messages;
    assert_eq!(
        sent.len(),
        4,
        "additionalContext + prompt + style reminder + total_tokens; got {sent:?}"
    );
    assert!(
        is_additional_context(&sent[0]),
        "leading meta; got {:?}",
        sent[0]
    );
    assert_eq!(text_of(&sent[1]), "streaming prompt");
    assert!(
        is_reminder(&sent[2], LEARNING_REMINDER),
        "the style reminder must be the byte-exact Learning reminder; got {:?}",
        sent[2]
    );
    assert!(
        text_of(&sent[3]).contains("<total_tokens>"),
        "total-tokens reminder trails the batch; got {:?}",
        sent[3]
    );

    // STORED history: reminder absent.
    let history = orch.session.lock().await.history.clone();
    assert!(
        history.iter().all(|m| !is_reminder(m, LEARNING_REMINDER)),
        "the reminder must never enter stored history; got {history:?}"
    );
    assert_eq!(text_of(&history[0]), "streaming prompt");

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
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(), // output_style: None
        Arc::new(MockApiClient::new(vec![])),
        streaming.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );

    orch.run_turn_streaming("only prompt")
        .await
        .expect("streaming turn");

    let calls = streaming.captured_calls().await;
    assert_eq!(calls.len(), 1);
    // No output-style reminder. The leading additional-context meta
    // (always present via `# currentDate`) prepends the prompt, and the
    // `total_tokens_reminder` trails it — that reminder defaults ON, as it
    // does in a stock Claude Code session, so it is part of every outgoing
    // list now. See `crate::prompt::total_tokens`.
    assert_eq!(
        calls[0].messages.len(),
        3,
        "additionalContext + prompt + total_tokens_reminder; got {:?}",
        calls[0].messages
    );
    assert!(
        is_additional_context(&calls[0].messages[0]),
        "leading meta; got {:?}",
        calls[0].messages[0]
    );
    assert_eq!(text_of(&calls[0].messages[1]), "only prompt");
    assert!(
        text_of(&calls[0].messages[2]).contains("<total_tokens>"),
        "trailing total-tokens reminder; got {:?}",
        calls[0].messages[2]
    );
}
