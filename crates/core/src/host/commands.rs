//! Slash-command dispatch surface — implemented by `lingxi-commands`,
//! called by `lingxi-orchestrator` and the CLI binary.
//!
//! Introduced by M5-09 (commands surface). The trait lives here in
//! `core::host` so the orchestrator and CLI can depend on the dispatch
//! abstraction without pulling in `lingxi-commands` directly.

use async_trait::async_trait;

/// Result of [`SlashCommandDispatcher::dispatch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashDispatchResult {
    /// Handler ran and returned a display string. The host PRINTS this (e.g.
    /// `/help`, `/model`, builtins) — it is NOT fed back to the model. Mirrors
    /// claude-code's `type: "local" | "local-jsx"` commands.
    Handled {
        /// Text the dispatcher wants to surface to the user.
        display: String,
    },
    /// A prompt-expanding slash command (claude-code `type: "prompt"`: bundled
    /// skills like `/loop`, plus Markdown/Plugin prompt commands). The host must
    /// feed `prompt` to its `run_turn` path AS the user turn — running the model
    /// — rather than printing it. This is what makes a typed `/loop 5m /foo`
    /// actually schedule + execute, matching the binary which injects the
    /// `getPromptForCommand` result as the user message.
    RunAsTurn {
        /// The expanded prompt text to submit as the user turn.
        prompt: String,
    },
    /// Input did not start with `/` — treat as a regular user prompt.
    NotASlashCommand,
    /// Input started with `/` but the name is not registered.
    Unknown {
        /// The unknown command name (without leading `/`).
        name: String,
        /// The locked `"Unknown command: /{name}"` literal.
        display: String,
    },
}

/// Routes a raw `/<name> <args>` user input to a registered handler.
#[async_trait]
pub trait SlashCommandDispatcher: Send + Sync {
    /// Dispatch the raw input (with or without leading `/`) and return the
    /// handler's display string, or the locked unknown-command literal if
    /// the name is not registered.
    async fn dispatch(&self, raw: &str) -> SlashDispatchResult;
}
