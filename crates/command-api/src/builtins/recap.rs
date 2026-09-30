//! `/recap` — one-line session recap via an isolated, read-only side query.
//!
//! Probed from the real 2.1.198 binary (`strings -a … | grep -F
//! 'name:"recap"'`): a `type:"local"` command, no `argumentHint` (no
//! positional args — `ParsedSlashCommand::raw_args`/`positional_args` are
//! unused here, matching e.g. `/clear`/`/doctor`, unlike `/voice`'s
//! `argumentHint:"[hold|tap|off]"`). Description verbatim:
//! `"Generate a one-line session recap now"`.
//!
//! ## Spec (behavior)
//!
//! (The `generate_recap` `OrchestratorHandle` seam described in the historical
//! "GAP" note below has since LANDED — `ConversationOrchestrator` overrides it
//! via `sidequery::ForkedAgentRunner`, so this handler is fully wired.)
//!
//! On invocation: (1) check whether the session has had at least one
//! qualifying turn — an assistant message, or a user compact-summary message
//! (the synthetic post-`/compact` continuation turn) — and if not, return the
//! fixed [`NO_TURN_MESSAGE`] immediately (this half IS implemented, via
//! [`OrchestratorHandle::conversation_transcript`]). (2) Otherwise issue an
//! isolated, single-turn (`maxTurns=1`) forked/side LLM query using the
//! current conversation context, with all tool use denied and the fixed
//! prompt text documented on the `recap_prompt_text_is_byte_exact` test below.
//! The query must NOT write to the transcript and must NOT mutate
//! conversation history/cache (upstream `skipTranscript:true`,
//! `skipCacheWrite:true`) — `/recap` is read-only w.r.t. session state, unlike
//! `/compact`. (3) On success, the model's trimmed text-block output becomes
//! the display. (4) If the underlying query itself returned an API-error
//! assistant message, that error message's own text is the display (dynamic,
//! not a fixed string). (5) If the caller aborts mid-flight, the fixed
//! `"Recap cancelled."` text (see the `cancelled_message_is_byte_exact` test
//! below) is returned. (6) On any other internal failure, the fixed
//! [`GENERIC_FAILURE_MESSAGE`].
//!
//! ## Wiring: the `generate_recap` seam
//!
//! Steps (2)-(6) run through [`OrchestratorHandle::generate_recap`] — a
//! read-only side query that reuses the SAME single-turn `ForkedAgentRunner`
//! the autocompactor uses (tool-denied + single-turn + history-inert BY
//! CONSTRUCTION), so it never truncates/replaces history the way
//! [`OrchestratorHandle::force_compact`] would. This handler maps its outcome:
//! `RecapOutcome::Text` → the trimmed recap (items 3+4); `RecapOutcome::Cancelled`
//! → [`CANCELLED_MESSAGE`] (item 5); `Err(HandleError)` → the honest
//! [`GENERIC_FAILURE_MESSAGE`] (item 6). Step (1) — the no-qualifying-turn gate
//! — is still enforced here BEFORE the seam is called (via
//! [`OrchestratorHandle::conversation_transcript`]), so `NoTurn` is not a
//! `RecapOutcome` variant. No `AuthHandle` is needed (recap piggybacks on
//! whatever model/session is already authenticated and configured).
//!
//! Cancellation is wired but inert until command dispatch threads a Ctrl-C
//! token into the handler: the live `generate_recap` always uses a fresh
//! un-cancelled token, so `RecapOutcome::Cancelled` is currently unreachable via
//! the palette (the same limitation as `force_compact()` vs
//! `force_compact_with_cancel`).

use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::host::{OrchestratorHandle, RecapOutcome};
use lingxi_core::types::ConversationMessage;
use std::sync::Arc;

/// Fixed no-arg-invocation display when the session has had zero qualifying
/// turns yet. Byte-exact from the 2.1.198 binary.
const NO_TURN_MESSAGE: &str = "Nothing to recap yet — send a message first.";

/// Fixed display for any internal failure other than the no-turn gate
/// (fallback system-prompt rebuild throws, or the forked query itself
/// throws). Byte-exact from the 2.1.198 binary. This is also the ONLY
/// reachable outcome once the no-turn gate passes, until the
/// `generate_recap` primitive described in the module docs lands — an honest
/// gap marker rather than a fabricated success.
const GENERIC_FAILURE_MESSAGE: &str = "Couldn't generate a recap. Run with --debug for details.";

/// Fixed display when the recap side query is aborted mid-flight (spec item 5).
/// Byte-exact from the 2.1.198 binary. Reachable once command dispatch threads
/// a Ctrl-C token into the handler (today `generate_recap` always uses a fresh
/// un-cancelled token, so `RecapOutcome::Cancelled` is wired but inert — the
/// same limitation as `force_compact()` vs `force_compact_with_cancel`).
const CANCELLED_MESSAGE: &str = "Recap cancelled.";

/// Exact prefix `compaction::prompt::get_compact_user_summary_message` stamps
/// on the synthetic post-`/compact` continuation user turn (`"This session is
/// being continued from a previous conversation that ran out of context."`,
/// followed by the formatted summary). Duplicated here as a literal rather
/// than an import: `command-core`'s regular (non-test) dependency graph does
/// not include the `compaction` crate (it is dev-only, per
/// `commands/core/Cargo.toml`), and `Cargo.toml` is a shared file this task
/// must not edit. Used only to recognize the "qualifying turn" case (1) —
/// this handler never constructs or mutates such a message.
const COMPACT_SUMMARY_PREFIX: &str =
    "This session is being continued from a previous conversation that ran out of context.";

/// `/recap` handler.
///
/// Wires the real no-turn gate against the live transcript; the forked
/// side-query itself is a documented GAP (see module docs) — no
/// `lingxi_core::host::OrchestratorHandle` method exists yet to run it.
#[derive(Clone)]
pub struct RecapHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl RecapHandler {
    /// Construct a `RecapHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }

    /// Whether `history` contains at least one qualifying turn: an assistant
    /// message, or a user message that is the synthetic post-`/compact`
    /// summary continuation (recognized by [`COMPACT_SUMMARY_PREFIX`]).
    /// Plain user messages and bare system messages do not qualify — a recap
    /// needs either a model turn to summarize or a compaction summary
    /// standing in for the earlier portion of the conversation.
    fn has_qualifying_turn(history: &[ConversationMessage]) -> bool {
        history.iter().any(|m| match m {
            ConversationMessage::Assistant { .. } => true,
            ConversationMessage::User { .. } => {
                m.text_content().starts_with(COMPACT_SUMMARY_PREFIX)
            }
            ConversationMessage::System { .. } => false,
        })
    }
}

#[async_trait]
impl BuiltinCommandHandler for RecapHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        telemetry::emit_command_started("tengu_command_recap_started");

        let history = self.handle.conversation_transcript().await;
        if !Self::has_qualifying_turn(&history) {
            telemetry::emit_command_completed("tengu_command_recap_completed", "no-turn");
            return CommandResult::Done {
                display: Some(NO_TURN_MESSAGE.to_string()),
            };
        }

        // Run the isolated, tool-denied, transcript-skipping recap side query
        // via the `generate_recap` seam (the SAME single-turn forked-agent
        // runner the autocompactor uses — read-only, so it never mutates
        // history/cache the way `force_compact` would). Map its outcome:
        //   Text     → the model's trimmed recap (spec items 3+4)
        //   Cancelled→ the fixed "Recap cancelled." line (spec item 5)
        //   Err      → the fixed generic-failure message (spec item 6)
        match self.handle.generate_recap().await {
            Ok(RecapOutcome::Text(text)) => {
                telemetry::emit_command_completed("tengu_command_recap_completed", "ok");
                CommandResult::Done {
                    display: Some(text),
                }
            }
            Ok(RecapOutcome::Cancelled) => {
                telemetry::emit_command_completed("tengu_command_recap_completed", "cancelled");
                CommandResult::Done {
                    display: Some(CANCELLED_MESSAGE.to_string()),
                }
            }
            Err(e) => {
                telemetry::emit_command_failed("tengu_command_recap_failed", &e.to_string());
                CommandResult::Done {
                    display: Some(GENERIC_FAILURE_MESSAGE.to_string()),
                }
            }
        }
    }

    fn name(&self) -> &str {
        "recap"
    }

    fn description(&self) -> &str {
        "Generate a one-line session recap now"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::{ContentBlock, MessageId};
    use orchestrator::test_support::MockOrchestratorHandle;

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "recap".to_string(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    fn user_text(text: &str) -> ConversationMessage {
        ConversationMessage::user(MessageId::new(), text.to_string())
    }

    fn assistant_text(text: &str) -> ConversationMessage {
        ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            stop_reason: Some("end_turn".to_string()),
        }
    }

    /// Minimal local stub overriding `conversation_transcript` (the
    /// qualifying-turn gate's input) + `generate_recap` (the seam the handler
    /// calls once the gate passes). The shared `MockOrchestratorHandle` has no
    /// transcript setter, so this tiny in-file stub exercises the whole path
    /// without touching it. `recap = None` leaves `generate_recap` at its trait
    /// default (`Unimplemented` → the handler's `Err` → generic-failure) so the
    /// pre-seam fallback tests keep asserting that branch.
    struct TranscriptStub {
        history: Vec<ConversationMessage>,
        recap: Option<Result<RecapOutcome, lingxi_core::host::HandleError>>,
    }

    impl TranscriptStub {
        fn new(history: Vec<ConversationMessage>) -> Self {
            Self {
                history,
                recap: None,
            }
        }
        fn with_recap(
            history: Vec<ConversationMessage>,
            recap: Result<RecapOutcome, lingxi_core::host::HandleError>,
        ) -> Self {
            Self {
                history,
                recap: Some(recap),
            }
        }
    }

    #[async_trait]
    impl OrchestratorHandle for TranscriptStub {
        async fn current_session_id(&self) -> lingxi_core::types::SessionId {
            lingxi_core::types::SessionId::new()
        }
        async fn clear_session(&self) -> Result<(), lingxi_core::host::HandleError> {
            Ok(())
        }
        async fn force_compact(
            &self,
        ) -> Result<lingxi_core::host::CompactionSummary, lingxi_core::host::HandleError> {
            Ok(lingxi_core::host::CompactionSummary::default())
        }
        async fn snapshot_cost(&self) -> lingxi_core::host::CostSnapshot {
            lingxi_core::host::CostSnapshot::default()
        }
        async fn switch_model(
            &self,
            _model: &str,
            _profile: Option<&str>,
        ) -> Result<(), lingxi_core::host::HandleError> {
            Ok(())
        }
        async fn request_exit(&self) {}
        async fn current_should_exit(&self) -> bool {
            false
        }
        async fn open_memory_editor(
            &self,
        ) -> Result<lingxi_core::host::MemoryEditorOutcome, lingxi_core::host::HandleError>
        {
            Err(lingxi_core::host::HandleError::Unimplemented("stub".into()))
        }
        async fn list_mcp_servers(&self) -> Vec<lingxi_core::host::McpServerInfo> {
            Vec::new()
        }
        async fn list_skills(&self) -> Vec<lingxi_core::host::SkillInfo> {
            Vec::new()
        }
        async fn list_hooks(&self) -> Vec<lingxi_core::host::HookInfo> {
            Vec::new()
        }
        async fn list_agents(&self) -> Vec<lingxi_core::host::AgentInfo> {
            Vec::new()
        }
        async fn run_doctor_checks(&self) -> lingxi_core::host::DoctorReport {
            lingxi_core::host::DoctorReport::default()
        }
        async fn get_status_snapshot(&self) -> lingxi_core::host::StatusSnapshot {
            lingxi_core::host::StatusSnapshot::default()
        }
        async fn edit_config_file(
            &self,
        ) -> Result<lingxi_core::host::MemoryEditorOutcome, lingxi_core::host::HandleError>
        {
            Err(lingxi_core::host::HandleError::Unimplemented("stub".into()))
        }
        async fn edit_permissions_file(
            &self,
        ) -> Result<lingxi_core::host::MemoryEditorOutcome, lingxi_core::host::HandleError>
        {
            Err(lingxi_core::host::HandleError::Unimplemented("stub".into()))
        }
        async fn list_available_models(&self) -> Vec<String> {
            Vec::new()
        }
        async fn conversation_transcript(&self) -> Vec<ConversationMessage> {
            self.history.clone()
        }
        async fn generate_recap(&self) -> Result<RecapOutcome, lingxi_core::host::HandleError> {
            match &self.recap {
                Some(Ok(outcome)) => Ok(outcome.clone()),
                Some(Err(e)) => Err(e.clone()),
                None => Err(lingxi_core::host::HandleError::Unimplemented("stub".into())),
            }
        }
    }

    #[tokio::test]
    async fn no_turn_when_history_empty() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = RecapHandler::new(mock);
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Nothing to recap yet — send a message first.");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_turn_when_only_plain_user_messages() {
        let handle = Arc::new(TranscriptStub::new(vec![
            user_text("hello"),
            user_text("world"),
        ]));
        let h = RecapHandler::new(handle);
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Nothing to recap yet — send a message first.");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn qualifying_turn_with_unwired_recap_falls_back_to_generic_failure() {
        // The no-turn gate passes (there IS a qualifying turn) and the handler
        // calls `generate_recap`, but this stub leaves it unwired (`recap: None`
        // → the trait default `Unimplemented`), so the handler maps the `Err` to
        // the honest fixed generic-failure text — never a fabricated recap.
        let handle = Arc::new(TranscriptStub::new(vec![
            user_text("please fix the bug"),
            assistant_text("Fixed it."),
        ]));
        let h = RecapHandler::new(handle);
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(
                    s,
                    "Couldn't generate a recap. Run with --debug for details."
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn qualifying_turn_with_compact_summary_user_message() {
        // Post-/compact continuation turn (synthetic user message, no
        // assistant turn yet) also qualifies — recap should attempt to run
        // (and, pending the GAP, fall back to the generic failure) rather
        // than claim "nothing to recap".
        let summary_text =
            format!("{COMPACT_SUMMARY_PREFIX}\n\nSummary:\nWe were mid-refactor of the parser.");
        let handle = Arc::new(TranscriptStub::new(vec![user_text(&summary_text)]));
        let h = RecapHandler::new(handle);
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(
                    s,
                    "Couldn't generate a recap. Run with --debug for details."
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn qualifying_turn_wired_recap_renders_text() {
        // Gate passes AND the seam returns text → the handler renders that
        // trimmed text verbatim (spec items 3+4).
        let handle = Arc::new(TranscriptStub::with_recap(
            vec![user_text("please fix the bug"), assistant_text("Fixed it.")],
            Ok(RecapOutcome::Text(
                "Fixing the parser; next, run the tests.".to_string(),
            )),
        ));
        let h = RecapHandler::new(handle);
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Fixing the parser; next, run the tests.");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wired_recap_cancelled_renders_fixed_line() {
        // The seam reports a mid-flight abort → the fixed cancellation line
        // (spec item 5).
        let handle = Arc::new(TranscriptStub::with_recap(
            vec![assistant_text("Done.")],
            Ok(RecapOutcome::Cancelled),
        ));
        let h = RecapHandler::new(handle);
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Recap cancelled.");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wired_recap_error_renders_generic_failure() {
        // The seam fails internally → the fixed generic-failure text (spec item 6).
        let handle = Arc::new(TranscriptStub::with_recap(
            vec![assistant_text("Done.")],
            Err(lingxi_core::host::HandleError::ActionFailed("boom".into())),
        ));
        let h = RecapHandler::new(handle);
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(
                    s,
                    "Couldn't generate a recap. Run with --debug for details."
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn name_and_description() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = RecapHandler::new(mock);
        assert_eq!(h.name(), "recap");
        assert_eq!(h.description(), "Generate a one-line session recap now");
    }

    /// Byte-exact prompt text the forked side query sends — probed from the
    /// 2.1.198 binary. The production copy now lives on the impl side
    /// (`orchestrator::ConversationOrchestrator::RECAP_PROMPT`, `pub(crate)`);
    /// this byte-audit copy guards against drift in the shipped literal.
    const RECAP_PROMPT: &str = "The user stepped away and is coming back. Recap in under 40 words, 1-2 plain sentences, no markdown. Lead with the overall goal and current task, then the one next action. Skip root-cause narrative, fix internals, secondary to-dos, and em-dash tangents.";

    #[test]
    fn recap_prompt_text_is_byte_exact() {
        assert!(!RECAP_PROMPT.is_empty());
        assert!(RECAP_PROMPT.starts_with("The user stepped away and is coming back."));
        assert!(RECAP_PROMPT.ends_with("em-dash tangents."));
    }

    /// The production cancellation const (spec item 5) is byte-exact.
    #[test]
    fn cancelled_message_is_byte_exact() {
        assert_eq!(CANCELLED_MESSAGE, "Recap cancelled.");
    }
}
