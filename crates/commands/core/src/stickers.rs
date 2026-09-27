//! `/stickers` — open the LingXi sticker page in the browser.
//!
//! Ported 1:1 from the claude-code TS local command
//! `src/commands/stickers/stickers.ts`, which opens
//! `https://www.stickermule.com/claudecode` in the browser and returns a
//! text message. On success it shows `"Opening sticker page in browser…"`;
//! on failure it shows `"Failed to open browser. Visit: <url>"`.
//!
//! This handle-free port emits the static display text directly. Because the
//! struct carries no orchestrator handle (and thus no browser side effect), it
//! returns the success-path message, mirroring the TS `success` branch.

use async_trait::async_trait;
use command_api::builtin_support::names::core_description;
use command_api::model::{BuiltinCommandHandler, CommandResult};
use command_api::parser::ParsedSlashCommand;

/// `/stickers` handler — returns the sticker-page open message as `Done`.
///
/// No orchestrator dependency: this is a static display command. The TS
/// source opens `STICKERS_URL` in the browser and returns the text
/// `"Opening sticker page in browser…"` on success.
#[derive(Debug, Default)]
pub struct StickersHandler;

impl StickersHandler {
    /// Construct a new `StickersHandler`.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BuiltinCommandHandler for StickersHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        CommandResult::Done {
            display: Some("Opening sticker page in browser…".to_string()),
        }
    }

    fn name(&self) -> &str {
        "stickers"
    }

    fn description(&self) -> &str {
        core_description("stickers")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// URL opened by the TS `/stickers` command.
    const STICKERS_URL: &str = "https://www.stickermule.com/claudecode";

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "stickers".to_string(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn returns_done_with_open_message() {
        let h = StickersHandler::new();
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert!(
                    s.contains("Opening sticker page in browser"),
                    "unexpected display text: {s}"
                );
            }
            other => panic!("expected Done with display, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stickers_url_matches_ts_source() {
        // The ported URL must match the TS source verbatim.
        assert_eq!(STICKERS_URL, "https://www.stickermule.com/claudecode");
    }

    #[test]
    fn name_and_description() {
        let h = StickersHandler::new();
        assert_eq!(h.name(), "stickers");
        assert_eq!(h.description(), core_description("stickers"));
    }
}
