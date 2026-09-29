//! Source-annotated command descriptions for user-facing surfaces (typeahead,
//! `/help`, command palettes).
//!
//! Port of the TS `formatDescriptionWithSource` (`claude-code/src/commands.ts:730`),
//! which appends a parenthetical source tag to a command's description so users
//! can see where each command comes from. For model-facing prompts the TS uses
//! `cmd.description` directly; this helper is the *user*-facing variant.

use crate::model::{CommandSource, SlashCommand};

/// Format a command's description with its source annotation, for user-facing
/// UI. Mirrors TS `formatDescriptionWithSource`
/// (`claude-code/src/commands.ts:730`).
///
/// Resolution order, matching the TS branch order:
/// 1. A `loaded_from == "bundled"` entry gets a `(bundled)` suffix (TS
///    `cmd.source === 'bundled'`).
/// 2. `Builtin` and `Mcp` sources return the bare description — the TS returns
///    `cmd.description` unchanged for `source === 'builtin' || 'mcp'`.
/// 3. `Plugin` gets a `(plugin)` suffix. (The TS prefixes the plugin's manifest
///    name when available — `(name) desc` — but the manifest name is not carried
///    on the core model, so the core falls back to the TS `desc (plugin)` form.)
/// 4. The remaining setting-backed sources reuse the TS `getSettingSourceName`
///    mapping (`claude-code/src/utils/settings/constants.ts:26`): `User` →
///    `user`, `Project` → `project`, `Local` → `project, gitignored`, `Managed`
///    → `managed`.
#[must_use]
pub fn format_description_with_source(cmd: &SlashCommand) -> String {
    // TS: `if (cmd.source === 'bundled') return `${cmd.description} (bundled)``.
    // Bundled origin is carried via `loaded_from` (and, for programmatic
    // bundled skills, also via `CommandSource::Bundled` below).
    if cmd.loaded_from.as_deref() == Some("bundled") {
        return format!("{} (bundled)", cmd.description);
    }

    match cmd.source {
        // TS: `if (cmd.source === 'builtin' || cmd.source === 'mcp') return cmd.description`.
        // `Bundled` renders its bare description too (matched here only when
        // `loaded_from != "bundled"`, e.g. a programmatic skill without the tag).
        CommandSource::Builtin | CommandSource::Mcp | CommandSource::Bundled => {
            cmd.description.clone()
        }
        // TS: plugin → `(name) desc` when the manifest name is known, else `desc (plugin)`.
        CommandSource::Plugin => format!("{} (plugin)", cmd.description),
        // TS: `getSettingSourceName` mapping for the SettingSource cases.
        CommandSource::Settings(lingxi_core::types::SettingsScope::User) => {
            format!("{} (user)", cmd.description)
        }
        CommandSource::Settings(lingxi_core::types::SettingsScope::Project) => {
            format!("{} (project)", cmd.description)
        }
        CommandSource::Settings(lingxi_core::types::SettingsScope::Local) => {
            format!("{} (project, gitignored)", cmd.description)
        }
        CommandSource::Settings(lingxi_core::types::SettingsScope::Managed) => {
            format!("{} (managed)", cmd.description)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SlashCommandKind;

    fn cmd(name: &str, description: &str, source: CommandSource) -> SlashCommand {
        SlashCommand {
            name: name.to_string(),
            description: description.to_string(),
            source,
            kind: SlashCommandKind::Builtin {
                handler_id: name.to_string(),
            },
            ..SlashCommand::default()
        }
    }

    #[test]
    fn project_source_gets_project_suffix() {
        let c = cmd(
            "deploy",
            "Ship it",
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
        );
        assert_eq!(format_description_with_source(&c), "Ship it (project)");
    }

    #[test]
    fn user_source_gets_user_suffix() {
        let c = cmd(
            "note",
            "Jot a note",
            CommandSource::Settings(lingxi_core::types::SettingsScope::User),
        );
        assert_eq!(format_description_with_source(&c), "Jot a note (user)");
    }

    #[test]
    fn builtin_source_has_no_suffix() {
        // TS returns `cmd.description` unchanged for builtin (and mcp).
        let c = cmd("help", "Show help", CommandSource::Builtin);
        assert_eq!(format_description_with_source(&c), "Show help");
    }

    #[test]
    fn mcp_source_has_no_suffix() {
        let c = cmd("ask", "Ask the server", CommandSource::Mcp);
        assert_eq!(format_description_with_source(&c), "Ask the server");
    }

    #[test]
    fn plugin_source_gets_plugin_suffix() {
        let c = cmd("scan", "Scan repo", CommandSource::Plugin);
        assert_eq!(format_description_with_source(&c), "Scan repo (plugin)");
    }

    #[test]
    fn local_source_matches_ts_setting_name() {
        let c = cmd(
            "x",
            "Local cmd",
            CommandSource::Settings(lingxi_core::types::SettingsScope::Local),
        );
        assert_eq!(
            format_description_with_source(&c),
            "Local cmd (project, gitignored)"
        );
    }

    #[test]
    fn managed_source_matches_ts_setting_name() {
        let c = cmd(
            "x",
            "Managed cmd",
            CommandSource::Settings(lingxi_core::types::SettingsScope::Managed),
        );
        assert_eq!(format_description_with_source(&c), "Managed cmd (managed)");
    }

    #[test]
    fn bundled_loaded_from_overrides_source_suffix() {
        let mut c = cmd(
            "skill",
            "A bundled skill",
            CommandSource::Settings(lingxi_core::types::SettingsScope::User),
        );
        c.loaded_from = Some("bundled".to_string());
        assert_eq!(
            format_description_with_source(&c),
            "A bundled skill (bundled)"
        );
    }
}
