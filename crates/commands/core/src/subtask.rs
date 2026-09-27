//! `/subtask` — send a subagent off with your full context; its result comes
//! back here.
//!
//! Parity with claude-code v2.1.212. Upstream renamed the in-session
//! subagent-spawn command from `/fork` to `/subtask` and, when agent-view is
//! enabled (the default), repurposed `/fork` for a background session copy.
//! The command list (`Blr`) selects between them:
//!
//! ```text
//! ...vO()&&!IS_DEMO ? [vAd,RAd] : [SAd]
//! ```
//!
//! where `vO()` is true when agent-view is enabled. In the default branch the
//! set registers the redefined `/fork` (`vAd`) plus `/subtask` (`RAd`); the
//! agent-view-disabled fallback keeps the old `/fork` (`SAd`). `/subtask`
//! (`RAd` + handler `dF_`) is the spawn-a-subagent behavior that `/fork`
//! historically had:
//!
//! ```text
//! RAd={type:"local-jsx",name:"subtask",
//!   description:"Send a subagent off with your full context; its result comes
//!   back here",argumentHint:"<task>",isEnabled:()=>!hb(),load:...}
//!
//! dF_=async(e,t,r)=>{
//!   let n=r.trim();
//!   if(!n)return e("Usage: /subtask \<task\>",{display:"system"}),null;
//!   let o=await bZr(n,t,t.canUseTool??KO);
//!   if(!o)return e(hb()?"Subtasks are not available in coordinator sessions.
//!     Use /branch instead.":"Cannot start a subtask before the first
//!     conversation turn",{display:"system"}),null;
//!   return e(`${V9} forked ${o.name} (${o.agentId.slice(-4)})`,
//!     {display:"system"}),null;
//! };
//! ```
//!
//! `V9` = `"⑂"` (U+2442, the OCR-FORK control-picture glyph) — identical to the
//! old `/fork` success icon. `dF_` is byte-for-byte the old fork handler
//! (`Ptf`) with three strings reworded from "fork" to "subtask"; it drives the
//! same subagent spawn (`bZr`) and prints the same `⑂ forked name (id)` line.
//!
//! LingXi keeps its existing [`crate::ForkHandler`] (the old `SAd`/`/fork`
//! spawn) and adds this `/subtask` alongside it, reusing the already-ported
//! [`platform_api::OrchestratorHandle::fork_conversation`] spawn seam. The
//! control-flow restructuring (explicit early-exit gates on empty task /
//! coordinator session / no-first-turn) matches the `ForkHandler` port and is
//! behavior-preserving; see that module's docs for the rationale.

use async_trait::async_trait;
use command_api::model::{BuiltinCommandHandler, CommandResult};
use command_api::parser::ParsedSlashCommand;
use platform_api::{ForkOutcome, OrchestratorHandle};
use protocol::ConversationMessage;
use std::sync::Arc;

/// `${V9}` — the subagent-spawn success icon, U+2442 (OCR-FORK
/// control-picture glyph). Byte-exact with the claude-code v2.1.212 binary
/// (shared with the old `/fork` handler).
const FORK_ICON: char = '\u{2442}';

/// `/subtask` handler — spawns a subagent that inherits the full conversation
/// and reports its result back here (claude-code `subtask`, local-jsx
/// `dF_`/`RAd`).
#[derive(Clone)]
pub struct SubtaskHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl SubtaskHandler {
    /// Construct a `SubtaskHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for SubtaskHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        // `let n = r.trim(); if (!n) return e("Usage: /subtask <task>", ...)`.
        let task = args.raw_args.trim();
        if task.is_empty() {
            return CommandResult::Done {
                display: Some("Usage: /subtask \\<task\\>".to_string()),
            };
        }

        // `isEnabled:()=>!hb()` hides /subtask from the palette entirely in a
        // coordinator session; ported here as an explicit early gate (mirroring
        // `ForkHandler`) rather than a post-hoc branch on a failed spawn.
        if self.handle.is_coordinator_session().await {
            return CommandResult::Done {
                display: Some(
                    "Subtasks are not available in coordinator sessions. Use /branch instead."
                        .to_string(),
                ),
            };
        }

        // "Has had a first turn": the live transcript must end with an
        // assistant message — that message is what the subagent context is
        // built from.
        let transcript = self.handle.conversation_transcript().await;
        let has_first_turn = matches!(
            transcript.last(),
            Some(ConversationMessage::Assistant { .. })
        );
        if !has_first_turn {
            return CommandResult::Done {
                display: Some(
                    "Cannot start a subtask before the first conversation turn".to_string(),
                ),
            };
        }

        match self.handle.fork_conversation(task).await {
            Ok(ForkOutcome { name, agent_id }) => {
                // `o.agentId.slice(-4)` — last 4 chars (or fewer, for a shorter
                // id). Agent ids are ASCII, so a byte slice is safe.
                let tail = &agent_id[agent_id.len().saturating_sub(4)..];
                CommandResult::Done {
                    display: Some(format!("{FORK_ICON} forked {name} ({tail})")),
                }
            }
            Err(e) => {
                // No upstream error string is shown for this branch; mirror the
                // established `Could not <verb>: {msg}` prefix pattern
                // (`ForkHandler`) instead of inventing a byte-exact claim this
                // port can't verify.
                let msg = e.to_string();
                CommandResult::Done {
                    display: Some(format!("Could not start subtask: {msg}")),
                }
            }
        }
    }

    fn name(&self) -> &str {
        "subtask"
    }

    fn description(&self) -> &str {
        // Verbatim claude-code v2.1.212 command-object description.
        "Send a subagent off with your full context; its result comes back here"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;

    fn args(raw: &str) -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "subtask".to_string(),
            raw_args: raw.to_string(),
            positional_args: raw.split_whitespace().map(str::to_string).collect(),
        }
    }

    fn handler() -> SubtaskHandler {
        SubtaskHandler::new(Arc::new(MockOrchestratorHandle::new()))
    }

    #[tokio::test]
    async fn empty_args_render_usage() {
        for raw in ["", "   "] {
            match handler().handle(&args(raw)).await {
                CommandResult::Done { display: Some(s) } => {
                    assert_eq!(s, "Usage: /subtask \\<task\\>");
                }
                other => panic!("expected Done, got {other:?}"),
            }
        }
    }

    /// Default mock: `is_coordinator_session` and `conversation_transcript`
    /// both fall back to their `OrchestratorHandle` defaults (`false` /
    /// empty), so a non-empty task lands on the "no first turn yet" branch.
    #[tokio::test]
    async fn no_first_turn_yet_blocks_subtask() {
        match handler().handle(&args("Investigate the bug")).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(
                    s,
                    "Cannot start a subtask before the first conversation turn"
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn name_and_description() {
        let h = handler();
        assert_eq!(h.name(), "subtask");
        assert_eq!(
            h.description(),
            "Send a subagent off with your full context; its result comes back here"
        );
    }
}
