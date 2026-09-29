//! `/agents` — removed-wizard guidance (cc 2.1.198 M4).
//!
//! claude-code 2.1.198 REMOVED the `/agents` wizard (changelog "Removed
//! /agents wizard"). The command object is now (binary `Ltf` @ the `Hrc`
//! module): `{type:"local", name:"agents", description:"(removed) Ask Claude
//! to create/manage subagents, or edit .lingxi/agents/",
//! supportsNonInteractive:!0, load:()=>Promise.resolve({call:Otf})}`, where
//! `Otf` returns a static `{type:"text"}` guidance message (extracted verbatim
//! from the binary — see [`AGENTS_REMOVED_MESSAGE`]).
//!
//! lingxi previously rendered a list of registered subagents here; that list
//! surface is replaced by the binary's guidance text. BRANDING: the two
//! `agents/` paths are branded via [`branding::DOT_DIR`] (`.lingxi/agents/`)
//! because they point users at the dirs lingxi ACTUALLY loads
//! (`engine-desktop` (5.3) scans `<cwd>/.lingxi/agents` +
//! `<lingxi_home>/agents`); the docs URL stays verbatim. The command
//! DESCRIPTION stays byte-verbatim per the `core_description` convention
//! (command help strings are 1:1 with the oracle).

use crate::builtin_support::names::core_description;
use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::host::OrchestratorHandle;
use std::sync::Arc;
use telemetry::tengu::command as cmd_evt;

/// The `Otf` guidance text (binary 2.1.198, verbatim except the two branded
/// `agents/` paths — `.claude` → [`branding::DOT_DIR`]). The `\u{2022}`
/// bullets and column padding match the binary byte-for-byte.
fn agents_removed_message() -> String {
    format!(
        "The /agents wizard has been removed.\n\n\
         Ask Claude to create or update subagents for you (e.g. \"create a code-reviewer subagent that ...\"),\n\
         or edit the files directly:\n  \
         \u{2022} {dot}/agents/       (this project)\n  \
         \u{2022} ~/{dot}/agents/     (all projects)\n\n\
         Docs: https://code.claude.com/docs/en/sub-agents",
        dot = branding::DOT_DIR
    )
}

/// `/agents` handler — removed-wizard guidance (`supportsNonInteractive`, so
/// the same text serves the TUI and print/SDK paths).
#[derive(Clone)]
pub struct AgentsHandler;

impl AgentsHandler {
    /// Construct an `AgentsHandler`. The orchestrator handle is no longer
    /// consulted (the 2.1.198 command is a static text response), but the
    /// parameter is kept so the registration site (`register_core_batch_1`)
    /// stays signature-stable.
    #[must_use]
    pub fn new(_handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self
    }
}

#[async_trait]
impl BuiltinCommandHandler for AgentsHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        telemetry::emit_command_started(cmd_evt::AGENTS_STARTED);
        let s = agents_removed_message();
        telemetry::emit_command_completed(cmd_evt::AGENTS_COMPLETED, "");
        CommandResult::Done { display: Some(s) }
    }
    fn name(&self) -> &str {
        "agents"
    }
    fn description(&self) -> &str {
        core_description("agents")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "agents".into(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    /// The 2.1.198 guidance text, byte-locked (binary `Otf`, `.lingxi`-branded
    /// paths per the module doc).
    #[tokio::test]
    async fn returns_removed_wizard_guidance() {
        let h = AgentsHandler::new(Arc::new(MockOrchestratorHandle::new()));
        let locked = "The /agents wizard has been removed.\n\nAsk Claude to create or update subagents for you (e.g. \"create a code-reviewer subagent that ...\"),\nor edit the files directly:\n  \u{2022} .lingxi/agents/       (this project)\n  \u{2022} ~/.lingxi/agents/     (all projects)\n\nDocs: https://code.claude.com/docs/en/sub-agents";
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(s, locked);
        } else {
            panic!("expected Done with display text");
        }
    }

    /// Args are ignored — the binary's `Otf` takes none into account.
    #[tokio::test]
    async fn args_are_ignored() {
        let h = AgentsHandler::new(Arc::new(MockOrchestratorHandle::new()));
        let with_args = ParsedSlashCommand {
            name: "agents".into(),
            raw_args: "reviewer --verbose".into(),
            positional_args: vec!["reviewer".into(), "--verbose".into()],
        };
        let a = h.handle(&args()).await;
        let b = h.handle(&with_args).await;
        match (a, b) {
            (
                CommandResult::Done { display: Some(x) },
                CommandResult::Done { display: Some(y) },
            ) => assert_eq!(x, y),
            _ => panic!("expected Done for both"),
        }
    }

    /// Name + the byte-verbatim 2.1.198 description (oracle: `(removed) Ask
    /// Claude to create/manage subagents, or edit .lingxi/agents/`).
    #[tokio::test]
    async fn name_and_description() {
        let h = AgentsHandler::new(Arc::new(MockOrchestratorHandle::new()));
        assert_eq!(h.name(), "agents");
        assert_eq!(
            h.description(),
            "(removed) Ask Claude to create/manage subagents, or edit .lingxi/agents/"
        );
    }
}
