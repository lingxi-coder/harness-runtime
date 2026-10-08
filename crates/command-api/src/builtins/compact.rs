//! `/compact` — runs a forced compaction pass and reports the summary.
//!
//! See plan `docs/superpowers/plans/2026-05-25-m5-10-commands-batch-1.md`
//! Task 4.

use crate::builtin_support::names::core_description;
use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::host::OrchestratorHandle;
use std::sync::Arc;
use telemetry::tengu::command as cmd_evt;

/// The user-visible `/compact` result, byte-faithful to claude-code's
/// non-verbose `buildDisplayText` (`commands/compact/compact.ts:247`):
/// `Compacted (ctrl+o to see full summary)`. `ctrl+o` is the
/// `app:toggleTranscript` binding (LingXi wires the same toggle in `tui::root`).
const COMPACT_DISPLAY: &str = "Compacted (ctrl+o to see full summary)";

/// `/compact` handler — calls
/// [`OrchestratorHandle::force_compact`](lingxi_core::host::OrchestratorHandle::force_compact)
/// and renders the summary template.
#[derive(Clone)]
pub struct CompactHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl CompactHandler {
    /// Construct a `CompactHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for CompactHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        telemetry::emit_command_started(cmd_evt::COMPACT_STARTED);
        match self
            .handle
            .force_compact_with_instructions(&args.raw_args)
            .await
        {
            Ok(summary) => {
                let details = format!(
                    "{{\"messages_before\":{},\"messages_after\":{},\"bytes_saved\":{}}}",
                    summary.messages_before, summary.messages_after, summary.bytes_saved
                );
                telemetry::emit_command_completed(cmd_evt::COMPACT_COMPLETED, &details);
                // Byte-faithful to claude-code's `buildDisplayText`
                // (`commands/compact/compact.ts:230-247`): the user-visible result
                // is `chalk.dim('Compacted ' + dimmed.join('\n'))` where, in the
                // non-verbose case, `dimmed = ['(' + expandShortcut + ' to see full
                // summary)']` and `expandShortcut` = the `app:toggleTranscript`
                // binding (`ctrl+o`). claude-code shows NO message counts here — the
                // delta is recorded in telemetry above and the full summary is
                // reachable via the ctrl+o transcript toggle (already wired in
                // `tui::root`). Dimming is applied by the TUI render layer (LingXi
                // command displays are plain strings), not embedded here.
                CommandResult::Done {
                    display: Some(COMPACT_DISPLAY.to_string()),
                }
            }
            Err(e) => {
                let msg = e.to_string();
                telemetry::emit_command_failed(cmd_evt::COMPACT_FAILED, &msg);
                CommandResult::Done {
                    display: Some(compact_failure_display(&msg)),
                }
            }
        }
    }

    fn name(&self) -> &str {
        "compact"
    }

    fn description(&self) -> &str {
        core_description("compact")
    }
}

/// Map an orchestrator compaction-failure message onto CC 2.1.217's per-class
/// user-visible display (binary-verified, shared by the headless `/compact`
/// handler here and the TUI's off-loop compact closure):
///
/// - `Not enough messages to compact.` (`dQt`), `No messages to compact`, and
///   `Conversation too long…` (`Ito`) surface verbatim.
/// - A PreCompact hook block surfaces `Compaction blocked by PreCompact hook:
///   <blockedBy>` VERBATIM — CC throws `new tz(`${pQt}: ${e.blockedBy}`)` and
///   displays the tz message including the reason; stripping it would hide the
///   hook's own explanation.
/// - The user-abort sentinel (`compaction cancelled`, the orchestrator's exact
///   internal string) → `Compaction canceled.` (CC `signal.aborted` →
///   `Xl("Compaction canceled.")`). Matched on the full sentinel, not a bare
///   `cancelled`, so an upstream error that merely CONTAINS the word (e.g.
///   `request cancelled by upstream`) is not misreported as a user abort.
/// - A failed compaction stream surfaces CC's fixed interruption notice:
///   `Compaction interrupted · This may be due to network issues — please try
///   again.` (`Oks` in the 2.1.217 binary).
/// - `Error during compaction: <detail>` (manual-path `tz` errors, `Juy`'s
///   catch) surfaces verbatim from that substring on.
/// - The auto path's `Failed to generate conversation summary…` surfaces
///   verbatim.
/// - Anything else → the fixed `Error compacting conversation` notification
///   string.
#[must_use]
pub fn compact_failure_display(msg: &str) -> String {
    if msg.contains("Not enough messages to compact.") {
        return "Not enough messages to compact.".to_string();
    }
    if msg.contains("No messages to compact") {
        return "No messages to compact".to_string();
    }
    if msg.contains("Conversation too long.") {
        return "Conversation too long. Press esc twice to go up a few messages and try again."
            .to_string();
    }
    if let Some(i) = msg.find("Compaction blocked by PreCompact hook") {
        return msg[i..].to_string();
    }
    if msg.contains("compaction cancelled") {
        return "Compaction canceled.".to_string();
    }
    if msg.contains("Compaction interrupted") {
        return "Compaction interrupted · This may be due to network issues — please try again."
            .to_string();
    }
    for notice in [
        "Compaction failed · conversation could not be reduced below the context limit",
        "Compaction failed · attached media exceeds size limits",
    ] {
        if msg.contains(notice) {
            return notice.to_string();
        }
    }
    if let Some(i) = msg.find("Error during compaction:") {
        return msg[i..].to_string();
    }
    if msg.contains("Failed to generate conversation summary") {
        return "Failed to generate conversation summary - response did not contain valid text content"
            .to_string();
    }
    "Error compacting conversation".to_string()
}

/// Whether Claude renders a `/compact` failure result at error severity.
///
/// Its short-history sentinel is returned as ordinary text; the remaining
/// failure classes use the error presentation path.
#[must_use]
pub fn compact_failure_is_error(msg: &str) -> bool {
    !msg.contains("Not enough messages to compact.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::CompactionSummary;
    use orchestrator::test_support::MockOrchestratorHandle;

    #[test]
    fn latest_reactive_failure_notices_are_preserved() {
        // Claude Code 2.1.261 /compact exhausted + media_unstrippable cases.
        for expected in [
            "Compaction failed · conversation could not be reduced below the context limit",
            "Compaction failed · attached media exceeds size limits",
        ] {
            assert_eq!(
                compact_failure_display(&format!("handle action failed: {expected}")),
                expected
            );
            assert!(compact_failure_is_error(expected));
        }
    }

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "compact".to_string(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    /// `/compact <focus>` must forward its raw args as the summarizer's custom
    /// instructions — the trait DEFAULT delegates to plain `force_compact()`
    /// and drops them, so without this assertion a regression is invisible to
    /// every other test in this file (they all pass empty raw_args).
    #[tokio::test]
    async fn raw_args_forward_as_custom_instructions() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = CompactHandler::new(mock.clone());
        let mut a = args();
        a.raw_args = "focus on the API changes".to_string();
        let _ = h.handle(&a).await;
        assert_eq!(
            mock.last_compact_instructions().as_deref(),
            Some("focus on the API changes"),
            "raw_args must reach force_compact_with_instructions"
        );
    }

    #[tokio::test]
    async fn success_renders_compacted_hint() {
        // claude-code shows the brief `Compacted (ctrl+o to see full summary)`
        // regardless of the delta — the counts ride on telemetry, not the display.
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_compact_summary(CompactionSummary {
            messages_before: 42,
            messages_after: 7,
            bytes_saved: 18_345,
            summary: "kept context".to_string(),
        });
        let h = CompactHandler::new(mock);
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Compacted (ctrl+o to see full summary)");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failure_shows_fixed_error_text() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_compact_error("model 429".to_string());
        let h = CompactHandler::new(mock);
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                // claude shows the fixed "Error compacting conversation" — the
                // underlying error goes to telemetry, not the display.
                assert_eq!(s, "Error compacting conversation");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn short_history_surfaces_exact_message() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_compact_error("Not enough messages to compact.".to_string());
        let h = CompactHandler::new(mock);
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Not enough messages to compact.");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exhausted_ptl_retry_surfaces_exact_message() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_compact_error("Conversation too long.".to_string());
        let h = CompactHandler::new(mock);
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => assert_eq!(
                s,
                "Conversation too long. Press esc twice to go up a few messages and try again."
            ),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn precompact_block_surfaces_exact_message() {
        // The hook's own reason rides VERBATIM (CC displays the tz message
        // `Compaction blocked by PreCompact hook: <blockedBy>` unstripped).
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_compact_error("Compaction blocked by PreCompact hook: policy".to_string());
        let h = CompactHandler::new(mock);
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Compaction blocked by PreCompact hook: policy");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn failure_display_maps_every_error_class() {
        // Wrapped in the HandleError Display prefix, as the handler sees them.
        let f = |s: &str| compact_failure_display(&format!("handle action failed: {s}"));
        assert_eq!(
            f("Not enough messages to compact."),
            "Not enough messages to compact."
        );
        assert_eq!(f("No messages to compact"), "No messages to compact");
        assert_eq!(
            f("Conversation too long. Press esc twice to go up a few messages and try again."),
            "Conversation too long. Press esc twice to go up a few messages and try again."
        );
        // Hook reason rides verbatim.
        assert_eq!(
            f("Compaction blocked by PreCompact hook: budget exceeded"),
            "Compaction blocked by PreCompact hook: budget exceeded"
        );
        // Exact user-abort sentinel → CC's canceled string…
        assert_eq!(f("compaction cancelled"), "Compaction canceled.");
        assert_eq!(
            f("Compaction interrupted · This may be due to network issues — please try again."),
            "Compaction interrupted · This may be due to network issues — please try again."
        );
        // …but an upstream error merely CONTAINING 'cancelled' is NOT an abort.
        assert_eq!(
            f("compaction failed: request cancelled by upstream"),
            "Error compacting conversation"
        );
        // Manual-path tz errors surface from the marker on.
        assert_eq!(
            f("compaction failed: internal: Error during compaction: summarization produced empty response"),
            "Error during compaction: summarization produced empty response"
        );
        assert_eq!(
            f("compaction failed: internal: Failed to generate conversation summary - response did not contain valid text content"),
            "Failed to generate conversation summary - response did not contain valid text content"
        );
        assert_eq!(f("anything else"), "Error compacting conversation");
    }

    #[test]
    fn short_history_is_the_only_non_error_failure_notice() {
        assert!(!compact_failure_is_error(
            "handle action failed: Not enough messages to compact."
        ));
        assert!(compact_failure_is_error(
            "handle action failed: Compaction interrupted · This may be due to network issues — please try again."
        ));
        assert!(compact_failure_is_error("handle action failed: model 429"));
    }

    #[tokio::test]
    async fn zero_messages_renders_correctly() {
        // Even a no-op compaction shows the same fixed hint (claude-code does not
        // special-case the zero delta in the command result).
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_compact_summary(CompactionSummary {
            messages_before: 0,
            messages_after: 0,
            bytes_saved: 0,
            summary: String::new(),
        });
        let h = CompactHandler::new(mock);
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Compacted (ctrl+o to see full summary)");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn name_and_description() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = CompactHandler::new(mock);
        assert_eq!(h.name(), "compact");
        assert_eq!(
            h.description(),
            "Free up context by summarizing the conversation so far"
        );
    }

    /// An unwired real orchestrator must not turn the legacy deterministic
    /// summary fallback into a user-visible `/compact` success. Production
    /// composition wires a real `ForkedAgentRunner`; its model-call gate is
    /// covered by `orchestrator/tests/force_compact_real.rs`.
    #[tokio::test]
    async fn unwired_real_orchestrator_does_not_claim_success() {
        use compaction::CompactionOrchestrator;
        use lingxi_core::types::{ConversationMessage, MessageId};
        use orchestrator::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        use orchestrator::{ConversationOrchestrator, OrchestratorConfig};

        let api = Arc::new(MockApiClient::new(vec![]));
        let tools = Arc::new(tool_api::registry::ToolRegistry::new());
        let hooks = noop_hook_executor();
        let perms = Arc::new(NoOpPermissionGate);
        let output = Arc::new(MockOutputStream::new());
        let memory = Arc::new(StaticMemoryProvider::empty());
        let orch = Arc::new(
            ConversationOrchestrator::new(
                OrchestratorConfig::default(),
                api,
                tools,
                hooks,
                perms,
                output,
                memory,
                std::env::temp_dir(),
            )
            .with_compaction(Arc::new(CompactionOrchestrator::new(1_000))),
        );
        {
            let session = orch.session();
            let mut s = session.lock().await;
            for i in 0..40 {
                if i % 2 == 0 {
                    s.history.push(ConversationMessage::user(
                        MessageId::new(),
                        format!(
                            "msg-{i} body padding to push token count past the autocompact threshold"
                        ),
                    ));
                } else {
                    s.history.push(ConversationMessage::Assistant { per_turn_effort: None,
                        id: MessageId::new(),
                        content: vec![lingxi_core::types::ContentBlock::Text {
                            text: format!("reply-{i}"), citations: None,
                        }],
                        stop_reason: Some("end_turn".into()),
                    });
                }
            }
        }

        let handle: Arc<dyn OrchestratorHandle> = orch.clone();
        let h = CompactHandler::new(handle);
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(
                    s, "Error during compaction: no forked summarizer wired",
                    "got: {s}"
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
        let remaining = orch.session().lock().await.history.len();
        assert_eq!(remaining, 40, "failed compact must preserve history");
    }
}
