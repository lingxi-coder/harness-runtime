//! `/fork` — two variants, gated by the agent-view enablement flag (cc2.1.212).
//!
//! * [`ForkBackgroundHandler`] (`vAd`) — the **default** (agent-view enabled):
//!   copy the current conversation into a NEW BACKGROUND session and keep
//!   working here. See that type's docs for the byte-exact command object.
//! * [`ForkHandler`] (`SAd`, below) — the agent-view-**disabled** fallback:
//!   spawn a background agent that inherits the full conversation. The
//!   in-session subagent-spawn behaviour it historically carried by default
//!   moved to [`crate::builtins::SubtaskHandler`] (`/subtask`, `RAd`).
//!
//! [`crate::builtins::register_core_batch_8`](crate::builtins::register_core_batch_8) selects
//! the surface with `platform_api::agent_view::is_enabled()` exactly like the binary
//! command list `Blr` (`...vO()&&!IS_DEMO ? [vAd,RAd] : [SAd]`).
//!
//! ---
//!
//! `ForkHandler` (`SAd`) is a 1:1 port of the claude-code `type: 'local-jsx'`
//! command `fork`, verified against the shipped `claude.exe` v2.1.198 (compiled
//! names `xrc`/`Ptf`):
//!
//! ```text
//! xrc={type:"local-jsx",name:"fork",description:"Spawn a background agent
//! that inherits the full conversation",argumentHint:"<directive>",
//! isEnabled:()=>!tv(),load:...}
//!
//! Ptf=async(e,t,n)=>{
//!   let r=n.trim();
//!   if(!r)return e("Usage: /fork \<directive\>",{display:"system"}),null;
//!   let o=await t$o(r,t,t.canUseTool??ND);
//!   if(!o)return e(tv()?"Forking is not available in coordinator sessions.
//!     Use /branch instead.":"Cannot fork before the first conversation turn",
//!     {display:"system"}),null;
//!   return e(`${Hnt} forked ${o.name} (${o.agentId.slice(-4)})`,
//!     {display:"system"}),null;
//! };
//! ```
//!
//! `Hnt` = `"⑂"` (U+2442, the OCR-FORK control-picture glyph) — byte-exact
//! in [`FORK_ICON`].
//!
//! ## Control-flow restructuring (headless, behavior-preserving)
//!
//! claude's `Ptf` *attempts* the fork first (`t$o(...)`) and only picks
//! between the two failure messages afterward, branching on `tv()`
//! (coordinator-mode) at that point. This headless port has no local-jsx
//! dialog surface to drive that two-step shape, so it restructures the same
//! three outcomes into explicit early-exit gates: empty directive → usage;
//! coordinator session → the `/branch` pointer; no prior turn → the
//! "cannot fork yet" notice; otherwise attempt the spawn. This changes no
//! observable behavior: `isEnabled:()=>!tv()` already keeps `/fork` out of
//! the palette in a coordinator session (so `t$o` is never reached with
//! `tv()` true via normal dispatch), and gating up-front just avoids a
//! doomed spawn attempt when the command is invoked directly.
//!
//! ## Trait surface (integration pass — now LANDED)
//!
//! The two [`platform_api::OrchestratorHandle`] methods below are now real:
//! `is_coordinator_session` (default) + a live `fork_conversation` override on
//! `ConversationOrchestrator` that spawns the fork via the `SubagentSpawner`.
//! The historical gap note is retained for design rationale.
//!
//! Two [`platform_api::OrchestratorHandle`] methods this handler calls do not
//! exist on the trait yet:
//!
//!   * `async fn is_coordinator_session(&self) -> bool` — new additive
//!     default-`false` method, same shape as the existing
//!     `emit_coordinator_status` default.
//!   * `async fn fork_conversation(&self, directive: &str) ->
//!     Result<ForkOutcome, HandleError>` — new method. A benign additive
//!     default should return `Err(HandleError::Unimplemented("fork_conversation".into()))`,
//!     matching the documented purpose of the existing `Unimplemented`
//!     variant ("used by ... default impls ... so existing handle
//!     implementations don't need to override"). The concrete override
//!     belongs in the composition roots (`apps/engine-desktop`,
//!     `apps/engine-mobile`), delegating to the already-ported
//!     `platform_api::fork_subagent::build_forked_messages` /
//!     `build_child_message` plus the existing `BackgroundAgentSpawner` /
//!     `SubagentSpawner::spawn_async` lifecycle — this handler intentionally
//!     does not touch either (composition-root concern, not `commands/core`).
//!   * `platform_api::ForkOutcome { name: String, agent_id: String }` — new plain
//!     struct alongside `HandleError` in `platform_api::orchestrator`, re-exported
//!     from the crate root next to it.
//!
//! Until those three land this module will not compile on its own — by
//! design, per the multi-agent wiring split (this file is only the handler;
//! a separate integration pass adds the trait surface + registration).
//!
//! The `/branch` pointer in the coordinator message is otherwise dead in
//! LingXi today (`/branch` is currently registered as an interactive-only
//! command, not a real headless one), but the string is still correct
//! guidance for the (identical) interactive surface, so it is ported
//! byte-exact rather than reworded.

use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use platform_api::{ForkOutcome, OrchestratorHandle};
use protocol::ConversationMessage;
use std::sync::Arc;

/// `${Hnt}` — the fork-success icon, U+2442 (OCR-FORK control-picture
/// glyph). Byte-exact with the claude-code v2.1.198 binary.
const FORK_ICON: char = '\u{2442}';

/// `/fork` handler — spawns a background agent that inherits the full
/// conversation (claude-code `commands/fork`, local-jsx `Ptf`/`xrc`).
#[derive(Clone)]
pub struct ForkHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl ForkHandler {
    /// Construct a `ForkHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for ForkHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        // `let r = n.trim(); if (!r) return e("Usage: /fork <directive>", ...)`.
        let directive = args.raw_args.trim();
        if directive.is_empty() {
            return CommandResult::Done {
                display: Some("Usage: /fork \\<directive\\>".to_string()),
            };
        }

        // `isEnabled:()=>!tv()` hides /fork from the palette entirely in a
        // coordinator session; ported here as an explicit early gate (see
        // module docs) rather than a post-hoc branch on a failed spawn.
        if self.handle.is_coordinator_session().await {
            return CommandResult::Done {
                display: Some(
                    "Forking is not available in coordinator sessions. Use /branch instead."
                        .to_string(),
                ),
            };
        }

        // "Has had a first turn": the live transcript must end with an
        // assistant message — that message is what the fork prefix is built
        // from. No new trait surface needed; `conversation_transcript` is an
        // existing default method (empty `Vec` when no session is tracked).
        let transcript = self.handle.conversation_transcript().await;
        let has_first_turn = matches!(
            transcript.last(),
            Some(ConversationMessage::Assistant { .. })
        );
        if !has_first_turn {
            return CommandResult::Done {
                display: Some("Cannot fork before the first conversation turn".to_string()),
            };
        }

        match self.handle.fork_conversation(directive).await {
            Ok(ForkOutcome { name, agent_id }) => {
                // `o.agentId.slice(-4)` — last 4 chars (or fewer, for a
                // shorter id). Agent ids are ASCII, so a byte slice is safe.
                let tail = &agent_id[agent_id.len().saturating_sub(4)..];
                CommandResult::Done {
                    display: Some(format!("{FORK_ICON} forked {name} ({tail})")),
                }
            }
            Err(e) => {
                // No upstream error string is shown for this branch in the
                // binary excerpt; mirror the established
                // `Could not <verb>: {msg}` prefix pattern (see
                // `model.rs`'s `Could not switch model: {msg}`) instead of
                // inventing a byte-exact claim this port can't verify.
                let msg = e.to_string();
                CommandResult::Done {
                    display: Some(format!("Could not fork conversation: {msg}")),
                }
            }
        }
    }

    fn name(&self) -> &str {
        "fork"
    }

    fn description(&self) -> &str {
        // Verbatim claude-code v2.1.198 command-object description.
        "Spawn a background agent that inherits the full conversation"
    }
}

/// `/fork` handler — the **default** (agent-view-enabled) 2.1.212 behaviour
/// (`vAd`): copy the current conversation into a NEW BACKGROUND session and keep
/// working in the live/interactive session. Distinct from the legacy
/// [`ForkHandler`] (`SAd`, subagent-spawn) which is the agent-view-**disabled**
/// fallback.
///
/// Byte-exact command object (claude-code v2.1.212, binary `CAd`/`vAd`):
///
/// ```text
/// vAd={type:"local-jsx",name:"fork",
///   description:"Copy this conversation into a new background session and keep
///   working here",argumentHint:"[prompt]",isEnabled:()=>!hb()}
/// ```
///
/// Note `vAd` carries **no** `load`/`call` in the binary — upstream's special
/// dispatcher intercepts `name === "fork"` and routes it through the background
/// (`--bg`/daemon) session-copy path (`jobs/<short>/state.json` supervisor +
/// `__bg-run` worker) while the foreground session stays interactive. The
/// `[prompt]` argument is OPTIONAL (seeds the background session's next turn),
/// so — unlike the legacy handler — there is no "Usage:" gate on empty input.
///
/// This handler drives that seam via
/// [`platform_api::OrchestratorHandle::fork_to_background_session`], which returns the
/// system line to display in the live session (the composition root owns the
/// exact text, since it knows the newly-minted background session id). Until a
/// composition root overrides that seam it returns `Unimplemented`, and this
/// handler surfaces the established `Could not <verb>: {msg}` fallback (see
/// `model.rs`).
#[derive(Clone)]
pub struct ForkBackgroundHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl ForkBackgroundHandler {
    /// Construct a `ForkBackgroundHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for ForkBackgroundHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        // `argumentHint:"[prompt]"` — the prompt is OPTIONAL; a bare `/fork`
        // copies the conversation into a background session with no seed turn.
        let prompt = args.raw_args.trim();
        match self.handle.fork_to_background_session(prompt).await {
            Ok(display) => CommandResult::Done {
                display: Some(display),
            },
            Err(e) => {
                // No upstream call handler exists for `vAd` (special dispatcher),
                // so no byte-exact failure string is recoverable; mirror the
                // established `Could not <verb>: {msg}` prefix pattern instead of
                // inventing an unverifiable claim.
                let msg = e.to_string();
                CommandResult::Done {
                    display: Some(format!("Could not fork to background session: {msg}")),
                }
            }
        }
    }

    fn name(&self) -> &str {
        "fork"
    }

    fn description(&self) -> &str {
        // Verbatim claude-code v2.1.212 command-object description (`vAd`).
        "Copy this conversation into a new background session and keep working here"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;

    fn args(raw: &str) -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "fork".to_string(),
            raw_args: raw.to_string(),
            positional_args: raw.split_whitespace().map(str::to_string).collect(),
        }
    }

    fn handler() -> ForkHandler {
        ForkHandler::new(Arc::new(MockOrchestratorHandle::new()))
    }

    #[tokio::test]
    async fn empty_args_render_usage() {
        for raw in ["", "   "] {
            match handler().handle(&args(raw)).await {
                CommandResult::Done { display: Some(s) } => {
                    assert_eq!(s, "Usage: /fork \\<directive\\>");
                }
                other => panic!("expected Done, got {other:?}"),
            }
        }
    }

    /// Default mock: `is_coordinator_session` and `conversation_transcript`
    /// both fall back to their `OrchestratorHandle` defaults (`false` /
    /// empty), so a non-empty directive lands on the "no first turn yet"
    /// branch without needing any new mock setter.
    #[tokio::test]
    async fn no_first_turn_yet_blocks_fork() {
        match handler().handle(&args("Investigate the bug")).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Cannot fork before the first conversation turn");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn whitespace_only_directive_is_treated_as_empty() {
        match handler().handle(&args("   \t  ")).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Usage: /fork \\<directive\\>");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn name_and_description() {
        let h = handler();
        assert_eq!(h.name(), "fork");
        assert_eq!(
            h.description(),
            "Spawn a background agent that inherits the full conversation"
        );
    }

    // ── ForkBackgroundHandler (vAd — the default agent-view-enabled /fork) ──

    fn bg_handler() -> ForkBackgroundHandler {
        ForkBackgroundHandler::new(Arc::new(MockOrchestratorHandle::new()))
    }

    #[tokio::test]
    async fn bg_name_and_byte_exact_description() {
        let h = bg_handler();
        assert_eq!(h.name(), "fork");
        assert_eq!(
            h.description(),
            "Copy this conversation into a new background session and keep working here"
        );
    }

    /// The `[prompt]` argument is optional — a bare `/fork` reaches the seam
    /// (no "Usage:" gate) and renders whatever system line it returns. The mock
    /// override returns a deterministic Ok line.
    #[tokio::test]
    async fn bg_empty_prompt_routes_to_background_seam() {
        match bg_handler().handle(&args("")).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(
                    s,
                    "Copied conversation into a new background session (mock-bg-abcd)."
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bg_with_prompt_routes_to_background_seam() {
        match bg_handler()
            .handle(&args("keep investigating the flake"))
            .await
        {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(
                    s,
                    "Copied conversation into a new background session (mock-bg-abcd)."
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }
}
