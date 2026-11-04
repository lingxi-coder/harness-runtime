//! The process-wide command environment is isolated from the library tests.

use command_api::builtin_support::UnimplementedCommandHandler;
use command_api::{CommandRegistry, CommandSource, SlashCommand, SlashCommandKind};
use std::sync::Arc;

#[test]
fn palette_includes_manual_only_and_filters_hidden_and_disabled_commands() {
    let mut registry = CommandRegistry::new();
    let command = |name: &str| SlashCommand {
        name: name.into(),
        description: format!("{name} cmd").into(),
        source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        kind: SlashCommandKind::Builtin {
            handler_id: name.to_string(),
        },
        ..SlashCommand::default()
    };
    let mut manual_only = command("manual-only");
    manual_only.disable_model_invocation = true;
    registry.register_command(manual_only);
    let mut hidden = command("internal");
    hidden.user_invocable = Some(false);
    registry.register_command(hidden);
    registry.register_builtin_handler(Arc::new(UnimplementedCommandHandler::new(
        "upgrade", "Upgrade",
    )));

    let palette_names = || {
        registry
            .palette_commands()
            .into_iter()
            .map(|command| command.name.string_at("").unwrap_or_default().to_owned())
            .collect::<Vec<_>>()
    };

    std::env::set_var("DISABLE_UPGRADE_COMMAND", "1");
    let palette = palette_names();
    assert!(palette.iter().any(|name| name == "manual-only"));
    assert!(!palette.iter().any(|name| name == "internal"));
    assert!(!palette.iter().any(|name| name == "upgrade"));

    std::env::remove_var("DISABLE_UPGRADE_COMMAND");
    assert!(palette_names().iter().any(|name| name == "upgrade"));
}
