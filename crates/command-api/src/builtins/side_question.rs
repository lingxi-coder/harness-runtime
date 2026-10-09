//! `/btw` — ask a quick side question answered by an isolated, read-only,
//! single-turn side query whose inherited tool schemas are all denied.
//!
//! claude-code (`commands/btw/btw.tsx` + `utils/sideQuestion.ts`) wraps the
//! user's question in a fixed `<system-reminder>` (you are a separate
//! lightweight agent, the main agent is NOT interrupted, NO tools, one-off
//! response, answer from context only) and issues a single-turn forked/side
//! query sharing the parent prompt cache. The fork keeps the parent's tool
//! schemas for cache identity, but the one-shot runner denies every call;
//! it then shows the markdown answer in a
//! throwaway dialog — the answer NEVER enters the LLM conversation history.
//!
//! This handler is the LingXi analog: it delegates to the
//! [`OrchestratorHandle::answer_side_question`] seam — the SAME history-inert
//! single-turn `ForkedAgentRunner` `/recap` uses (single-turn by construction),
//! differing only in the prompt (the wrapped question). The
//! answer is rendered as a transcript system line by the caller
//! (`CommandResult::Done { display }`), never appended to `session.history`, so
//! the load-bearing parity property is preserved.
//!
//! Delivered through the SAME `run_core_command` bridge `/recap` uses (a
//! throwaway current-thread `block_on` on the render-loop's blocking thread),
//! so the multi-second isolated query behaves exactly like `/recap`'s.

use crate::model::{trim_source_text, BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::host::{OrchestratorHandle, RecapOutcome};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::sync::Arc;

/// Shown when `/btw` is invoked with no question. (The TUI dispatcher also
/// guards the empty case before reaching the handler, so this is a defensive
/// fallback.)
const USAGE_MESSAGE: &str = "Usage: /btw <your question>";

/// Shown when the side query returned successfully but produced no text.
const NO_RESPONSE_MESSAGE: &str = "No response received";

/// Shown when the side query is aborted mid-flight.
const CANCELLED_MESSAGE: &str = "Side question cancelled.";

/// Appended to a `/btw` answer that wrote tool calls as text (2.1.269, oracle
/// `w` in `src_188613081.js`).
///
/// The side query has no tools, so anything shaped like a tool call in its
/// answer is invented — and an invented `Read` result reads exactly like a real
/// one. This catches the case where the model writes one anyway, because a
/// silently fabricated file listing is worse than a refusal.
const FABRICATED_TOOL_CALL_NOTICE: &str = "_/btw can't run tools: any tool calls or tool output shown above were not executed and may not reflect your actual files or data. Ask in the main conversation to check._";

/// Does `answer` contain something shaped like a tool call?
///
/// Oracle `tkn`/`phs` (`src_169588164.js`):
/// ``new RegExp(`<(?:${fwt})?(?:function_calls>|invoke name=)|</(?:${fwt})?(?:function_calls|invoke)>`)``
/// — an optional namespace prefix, then an opening `function_calls`/`invoke`
/// tag or either closing tag. Hand-rolled rather than adding a `regex`
/// dependency to this crate; the alternatives are few and fixed.
fn writes_tool_calls_as_text(answer: &Utf16JsonProjection) -> bool {
    let Some(units) = answer.string_units("") else {
        return false;
    };
    let starts_with_ascii = |offset: usize, value: &str| {
        units
            .get(offset..)
            .is_some_and(|suffix| suffix.starts_with(&value.encode_utf16().collect::<Vec<_>>()))
    };

    for (index, unit) in units.iter().enumerate() {
        if *unit != u16::from(b'<') {
            continue;
        }
        let mut body = index + 1;
        let is_close = units.get(body) == Some(&u16::from(b'/'));
        if is_close {
            body += 1;
        }

        // Skip an optional namespace prefix only when its source units match
        // the native ASCII grammar. Other units remain opaque model text.
        if let Some(colon) = units[body..]
            .iter()
            .position(|unit| *unit == u16::from(b':'))
        {
            if (1..=32).contains(&colon)
                && units[body..body + colon].iter().all(|unit| {
                    *unit <= 0x7f
                        && (char::from_u32(u32::from(*unit)).is_some_and(|character| {
                            character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                        }))
                })
            {
                body += colon + 1;
            }
        }

        if if is_close {
            starts_with_ascii(body, "function_calls>") || starts_with_ascii(body, "invoke>")
        } else {
            starts_with_ascii(body, "function_calls>") || starts_with_ascii(body, "invoke name=")
        } {
            return true;
        }
    }
    false
}

/// `/btw` handler.
///
/// Bound to an orchestrator handle; runs the real isolated side query via the
/// [`OrchestratorHandle::answer_side_question`] seam and renders the trimmed
/// answer as read-only system output.
#[derive(Clone)]
pub struct SideQuestionHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl SideQuestionHandler {
    /// Construct a `SideQuestionHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for SideQuestionHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        let question = trim_source_text(&args.raw_args);
        if question
            .string_units("")
            .is_some_and(|units| units.is_empty())
        {
            return CommandResult::Done {
                display: Some((USAGE_MESSAGE.to_string()).into()),
            };
        }
        telemetry::emit_command_started("tengu_command_btw_started");

        // Run the isolated, tool-denied side query via the SAME single-turn
        // forked-agent runner `/recap` uses (history-inert by construction — the
        // answer never enters `session.history`). Map its outcome:
        //   Text (non-empty) → the model's trimmed answer
        //   Text (empty)     → the fixed "No response received" line
        //   Cancelled        → the fixed "Side question cancelled." line
        //   Err              → an honest failure line
        match self.handle.answer_side_question(&question).await {
            Ok(RecapOutcome::Text(text))
                if !trim_source_text(&text)
                    .string_units("")
                    .is_some_and(|units| units.is_empty()) =>
            {
                telemetry::emit_command_completed("tengu_command_btw_completed", "ok");
                // 2.1.269: flag an answer that wrote tool calls as text, so the
                // reader knows nothing in it was executed.
                let display = if writes_tool_calls_as_text(&text) {
                    crate::model::source_concat([
                        crate::model::PromptPart::Source(&text),
                        crate::model::PromptPart::Text("\n\n"),
                        crate::model::PromptPart::Text(FABRICATED_TOOL_CALL_NOTICE),
                    ])
                } else {
                    text
                };
                CommandResult::Done {
                    display: Some(display),
                }
            }
            Ok(RecapOutcome::Text(_)) => {
                telemetry::emit_command_completed("tengu_command_btw_completed", "empty");
                CommandResult::Done {
                    display: Some((NO_RESPONSE_MESSAGE.to_string()).into()),
                }
            }
            Ok(RecapOutcome::Cancelled) => {
                telemetry::emit_command_completed("tengu_command_btw_completed", "cancelled");
                CommandResult::Done {
                    display: Some((CANCELLED_MESSAGE.to_string()).into()),
                }
            }
            Err(e) => {
                telemetry::emit_command_failed("tengu_command_btw_failed", &e.to_string());
                CommandResult::Done {
                    display: Some((format!("Couldn't answer side question: {e}")).into()),
                }
            }
        }
    }

    fn name(&self) -> &str {
        "btw"
    }

    fn description(&self) -> &str {
        "Ask a quick side question without interrupting the main conversation"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;

    fn args(raw: &str) -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "btw".into(),
            raw_args: raw.into(),
            positional_args: raw.split_whitespace().map(Into::into).collect(),
        }
    }

    #[tokio::test]
    async fn bare_invocation_renders_usage() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = SideQuestionHandler::new(mock);
        match h.handle(&args("   ")).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s.string_at(""), Some("Usage: /btw <your question>"))
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unwired_handle_renders_honest_failure() {
        // The mock leaves `answer_side_question` at its trait default
        // (`Unimplemented` → Err), so the handler renders the honest failure
        // line rather than a fabricated answer.
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = SideQuestionHandler::new(mock);
        match h.handle(&args("why is the sky blue?")).await {
            CommandResult::Done { display: Some(s) } => {
                assert!(
                    s.string_at("")
                        .is_some_and(|text| text.starts_with("Couldn't answer side question:")),
                    "got: {s:?}"
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn name_and_description() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = SideQuestionHandler::new(mock);
        assert_eq!(h.name(), "btw");
        assert_eq!(
            h.description(),
            "Ask a quick side question without interrupting the main conversation"
        );
    }
}

#[cfg(test)]
mod fabricated_tool_call_tests {
    use super::*;

    /// 2.1.269 — `/btw` has no tools, so a tool call in its answer is invented.
    /// The oracle's `phs` regex matches an opening `function_calls`/`invoke`
    /// tag or either closing tag, with an optional namespace prefix.
    #[test]
    fn the_detector_matches_every_oracle_form() {
        for answer in [
            "Let me check.\n<function_calls>\n<invoke name=\"Read\">",
            "<invoke name=\"Bash\">ls</invoke>",
            "text </function_calls> more",
            "</invoke>",
            // Namespaced, the `${fwt}` slot.
            "<function_calls>",
            "</invoke>",
            "<invoke name=\"Read\">",
        ] {
            assert!(
                writes_tool_calls_as_text(&Utf16JsonProjection::from(answer)),
                "must flag: {answer:?}"
            );
        }
    }

    /// An ordinary answer must not be flagged — a spurious notice on every
    /// reply would train the reader to ignore it.
    #[test]
    fn an_ordinary_answer_is_not_flagged() {
        for answer in [
            "The config lives in `settings.json`.",
            "Use `<div>` for the wrapper.",
            "Compare a < b and c > d.",
            "See the `invoke` method on that trait.",
            "function_calls is the name of the XML block.",
            "",
        ] {
            assert!(
                !writes_tool_calls_as_text(&Utf16JsonProjection::from(answer)),
                "must NOT flag: {answer:?}"
            );
        }
    }

    /// The notice is the oracle's `w`, byte for byte.
    #[test]
    fn the_notice_is_byte_exact() {
        assert_eq!(
            FABRICATED_TOOL_CALL_NOTICE,
            "_/btw can't run tools: any tool calls or tool output shown above were not executed and may not reflect your actual files or data. Ask in the main conversation to check._"
        );
    }
}
