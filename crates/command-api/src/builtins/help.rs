//! `/help` — emits the locked rendering of the builtin-command surface.
//!
//! See plan `docs/superpowers/plans/2026-05-25-m5-10-commands-batch-1.md`
//! Task 5.

use crate::builtin_support::help_render::render_help_screen;
use crate::builtin_support::names::core_description;
use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use telemetry::tengu::command as cmd_evt;

/// `/help` handler — pure function of the static command tables.
///
/// No orchestrator dependency: `/help` is a render of the byte-locked
/// `BUILTIN_COMMAND_NAMES` slice.
#[derive(Debug, Default)]
pub struct HelpHandler;

impl HelpHandler {
    /// Construct a new `HelpHandler`.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BuiltinCommandHandler for HelpHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        telemetry::emit_command_started(cmd_evt::HELP_STARTED);
        let s = render_help_screen();
        let details = format!("{{\"lines\":{}}}", s.matches('\n').count());
        telemetry::emit_command_completed(cmd_evt::HELP_COMPLETED, &details);
        CommandResult::Done {
            display: Some((s).into()),
        }
    }

    fn name(&self) -> &str {
        "help"
    }

    fn description(&self) -> &str {
        core_description("help")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceTextTestExt;

    #[tokio::test]
    async fn returns_done_with_render_help_screen_output() {
        let _env_guard = crate::builtin_support::names::ENV_LOCK.lock().unwrap();
        for (_, variable) in crate::builtin_support::names::ENV_DISABLE_GATED_COMMANDS {
            std::env::remove_var(*variable);
        }
        telemetry::test_clear_flag("tengu_cinder_relay");

        let h = HelpHandler::new();
        let args = ParsedSlashCommand {
            name: "help".into(),
            raw_args: String::new().into(),
            positional_args: vec![],
        };
        match h.handle(&args).await {
            CommandResult::Done { display: Some(s) } => {
                assert!(s.starts_with("Commands:\n"));
                // (M4 cc2.1.198) /agents carries the removed-wizard description.
                assert!(s.contains(core_description("agents")));
                // /reload-plugins stays visible (host-bound, no isHidden gate).
                // (x402 was removed from the command set — 0 hits in 2.1.216+.)
                assert!(s.contains("/reload-plugins"));
                // Hidden/disabled commands are filtered out of /help, and the
                // policy-only `/heapdump` object is absent from recognition.
                assert!(!s.contains("  /heapdump "));
                assert!(!s.contains("  /ant-trace "));
                // 77 newline-terminated lines: one header plus 76 visible command
                // rows (83 builtins minus 8 hidden commands, plus bundled `/doctor`).
                // `/commit` is omitted while `tengu_cinder_relay` defaults off.
                let text = s.string_at("").unwrap();
                assert!(
                    text.lines()
                        .any(|line| line.starts_with("  /team-onboarding ")),
                    "the source-visible Native team-onboarding command must appear"
                );
                assert_eq!(text.matches('\n').count(), 77);
                assert!(s.contains("  /bug "));
                assert!(s.contains("  /powerup "));
                assert!(s.contains("  /output-style "));
                assert!(!s.contains("  /version "));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn name_and_description() {
        let h = HelpHandler::new();
        assert_eq!(h.name(), "help");
        assert_eq!(h.description(), "Show help and available commands");
    }
}
