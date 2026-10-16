//! Source-preserving command projection for the `-p` stream-json initialize
//! response. This follows Native `T5(Jr())`: the caller supplies the already
//! assembled non-interactive command pool, and this module applies only the
//! user-invocable and skill-Off projection before producing wire DTOs.

use command_api::{
    CommandRegistry, CommandSource, NativeCommandType, NativeSlashKind, SlashCommand,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::collections::HashSet;

use crate::desktop::CommandVisibilitySnapshot;

/// Collect command descriptors for the Native `-p` initialize response.
///
/// `CommandRegistry::headless_init_commands` is the current request's
/// `bge(Pp(...))` pool: it preserves Native source order and has already
/// applied headless factory/availability gates. T5 then de-duplicates by the
/// raw `name` (first occurrence wins), drops `userInvocable === false`, and
/// applies the current skill-Off predicate. Hidden descriptors stay visible
/// here, matching T5; the REPL bridge's `o7r` hidden/safe filters do not apply
/// to this CLI control response.
pub(super) fn collect(
    registry: &CommandRegistry,
    visibility: &CommandVisibilitySnapshot,
) -> Vec<Utf16JsonProjection> {
    let entrypoint = std::env::var("CLAUDE_CODE_ENTRYPOINT").ok();
    let reserved_native_spellings = native_reserved_spellings(registry, entrypoint.as_deref());
    let mut seen_names = HashSet::new();
    let mut projected = Vec::new();

    for command in registry.headless_init_commands() {
        if !native_builtin_plugin_descriptor_visible(command, entrypoint.as_deref()) {
            continue;
        }
        let Some(raw_name_units) = command.name.string_units("") else {
            continue;
        };

        // Native Ui(e, "name") de-duplicates before the m9r user-invocable
        // predicate, so a hidden first duplicate suppresses later rows too.
        if !seen_names.insert(raw_name_units.clone()) {
            continue;
        }
        if command.user_invocable == Some(false) || visibility.command_is_off(command) {
            continue;
        }

        if let Some(dto) = project_command(command, &raw_name_units, &reserved_native_spellings) {
            projected.push(dto);
        }
    }

    projected
}

fn project_command(
    command: &SlashCommand,
    raw_name_units: &[u16],
    reserved_native_spellings: &HashSet<String>,
) -> Option<Utf16JsonProjection> {
    let name = command
        .user_facing_name
        .as_ref()
        .unwrap_or(&command.name)
        .clone();
    let primary_units = name.string_units("")?;
    let description = native_description(command);

    let mut aliases = command
        .aliases
        .iter()
        .filter(|alias| alias.string_units("").as_deref() != Some(primary_units.as_slice()))
        .cloned()
        .collect::<Vec<_>>();

    // Native xAn exposes a qualified canonical name as an alias. For an
    // unqualified non-builtin prompt it exposes the canonical spelling only
    // when it is a legal skill name and does not shadow a reserved builtin.
    if raw_name_units != primary_units.as_slice()
        && should_expose_raw_name_alias(command, raw_name_units, reserved_native_spellings)
        && !aliases
            .iter()
            .any(|alias| alias.string_units("").as_deref() == Some(raw_name_units))
    {
        aliases.push(projection_from_units(raw_name_units.to_vec())?);
    }

    let argument_hint = command
        .argument_hint
        .as_ref()
        .and_then(|hint| hint.string_units(""))
        .filter(|units| !units.is_empty())
        .and_then(projection_from_units)
        .unwrap_or_else(|| Utf16JsonProjection::from(""));

    let mut fields = vec![
        ("name".to_owned(), name),
        ("description".to_owned(), description),
        ("argumentHint".to_owned(), argument_hint),
    ];
    if !aliases.is_empty() {
        fields.push((
            "aliases".to_owned(),
            Utf16JsonProjection::array(aliases).ok()?,
        ));
    }
    if is_native_builtin(command) {
        fields.push((
            "builtin".to_owned(),
            Utf16JsonProjection::parse("true").ok()?,
        ));
    }

    Utf16JsonProjection::object(fields).ok()
}

fn should_expose_raw_name_alias(
    command: &SlashCommand,
    raw_name_units: &[u16],
    reserved_native_spellings: &HashSet<String>,
) -> bool {
    if raw_name_units.contains(&(b':' as u16)) {
        return true;
    }
    if command.native_type != Some(NativeCommandType::Prompt)
        || command.source == CommandSource::Mcp
        || is_native_builtin(command)
        || !is_native_skill_name(raw_name_units)
        || raw_name_units.starts_with(&"mcp__".encode_utf16().collect::<Vec<_>>())
    {
        return false;
    }

    let normalized = String::from_utf16_lossy(raw_name_units).to_ascii_lowercase();
    !reserved_native_spellings.contains(&normalized)
}

fn is_native_skill_name(units: &[u16]) -> bool {
    !units.is_empty()
        && units.iter().all(
            |unit| matches!(*unit, 0x41..=0x5a | 0x61..=0x7a | 0x30..=0x39 | 0x2e | 0x5f | 0x2d),
        )
}

fn is_native_builtin(command: &SlashCommand) -> bool {
    if command.native_type == Some(NativeCommandType::Local)
        && command.loaded_from.as_deref() == Some("plugin")
    {
        // Native JPe is checked before t4's non-prompt fallback.
        return false;
    }
    if command.native_type != Some(NativeCommandType::Prompt) {
        // Native t4 only distinguishes non-prompt MCP descriptors by
        // loadedFrom; every other non-prompt row is classified as builtin.
        return command.loaded_from.as_deref() != Some("mcp");
    }

    match command.source {
        CommandSource::Builtin | CommandSource::Bundled => true,
        CommandSource::Settings(_) | CommandSource::Plugin | CommandSource::Mcp => false,
    }
}

fn native_description(command: &SlashCommand) -> Utf16JsonProjection {
    let Some(description_units) = command.description.string_units("") else {
        return Utf16JsonProjection::from("");
    };
    if command.native_type != Some(NativeCommandType::Prompt) {
        return command.description.clone();
    }

    if command.native_slash_kind == Some(NativeSlashKind::Workflow) {
        return append_text(&description_units, " (dynamic workflow)");
    }

    if command.loaded_from.as_deref() == Some("syncedSkills") {
        return append_text(&description_units, " (claude.ai sync)");
    }

    match command.source {
        CommandSource::Builtin | CommandSource::Bundled | CommandSource::Mcp => {
            command.description.clone()
        }
        CommandSource::Plugin => {
            if let Some(plugin_name) = command.plugin_name.as_ref() {
                let mut units = "(".encode_utf16().collect::<Vec<_>>();
                units.extend(plugin_name.string_units("").unwrap_or_default());
                units.extend(") ".encode_utf16());
                units.extend(description_units);
                projection_from_units(units).unwrap_or_else(|| command.description.clone())
            } else {
                append_text(&description_units, " (plugin)")
            }
        }
        CommandSource::Settings(scope) => {
            let suffix = match scope {
                lingxi_core::types::SettingsScope::Managed => " (managed)",
                lingxi_core::types::SettingsScope::User => " (user)",
                lingxi_core::types::SettingsScope::Project => " (project)",
                lingxi_core::types::SettingsScope::Local => " (project, gitignored)",
            };
            append_text(&description_units, suffix)
        }
    }
}

fn native_reserved_spellings(
    registry: &CommandRegistry,
    entrypoint: Option<&str>,
) -> HashSet<String> {
    let mut reserved = native_builtin_plugin_spellings(entrypoint);
    for command in registry.list_all() {
        let is_mzo_or_q4_descriptor = command.native_catalog_order.is_some();
        let is_bundled_skill = command.source == CommandSource::Bundled
            && command
                .native_source
                .as_ref()
                .and_then(|source| source.string_at(""))
                == Some("built-in")
            && command.native_slash_kind != Some(NativeSlashKind::Workflow);
        if !(is_mzo_or_q4_descriptor || is_bundled_skill)
            || !native_builtin_plugin_descriptor_visible(command, entrypoint)
        {
            continue;
        }

        // Native KCe consumes descriptor name, display name, and aliases.
        // CommandRegistry's alias index can also contain unrelated host
        // registrations, so it is not part of the Native reserved set.
        if let Some(name) = command.name.string_units("") {
            insert_normalized_name(&mut reserved, &name);
        }
        if let Some(name) = command
            .user_facing_name
            .as_ref()
            .and_then(|name| name.string_units(""))
        {
            insert_normalized_name(&mut reserved, &name);
        }
        for alias in &command.aliases {
            if let Some(alias) = alias.string_units("") {
                insert_normalized_name(&mut reserved, &alias);
            }
        }
    }
    reserved
}

fn native_builtin_plugin_spellings(entrypoint: Option<&str>) -> HashSet<String> {
    let mut reserved = HashSet::new();
    let local_agent = entrypoint == Some("local-agent");
    let remote = entrypoint.is_some_and(|value| value.starts_with("remote"));

    // Native's builtin plugin initializer always loads plugin-authoring except
    // for local-agent. Claude Test is loaded only when both gates allow it.
    if !local_agent {
        reserved.insert("plugin-authoring".to_owned());
        if !remote {
            reserved.extend([
                "claude-test".to_owned(),
                "claude-test-execute".to_owned(),
                "claude-test-draft".to_owned(),
            ]);
        }
    }
    reserved
}

fn native_builtin_plugin_descriptor_visible(
    command: &SlashCommand,
    entrypoint: Option<&str>,
) -> bool {
    let is_plugin_authoring_name = command
        .name
        .string_units("")
        .is_some_and(|units| units.iter().copied().eq("plugin-authoring".encode_utf16()));
    let is_native_builtin_plugin_facade = command.native_catalog_order.is_none()
        && command.source == CommandSource::Bundled
        && command.loaded_from.as_deref() == Some("bundled")
        && command
            .native_source
            .as_ref()
            .and_then(|source| source.string_at(""))
            == Some("built-in")
        && is_plugin_authoring_name;

    !(entrypoint == Some("local-agent") && is_native_builtin_plugin_facade)
}

fn insert_normalized_name(reserved: &mut HashSet<String>, units: &[u16]) {
    if units.iter().all(|unit| *unit <= 0x7f) {
        reserved.insert(String::from_utf16_lossy(units).to_ascii_lowercase());
    }
}

fn append_text(source_units: &[u16], suffix: &str) -> Utf16JsonProjection {
    let mut units = source_units.to_vec();
    units.extend(suffix.encode_utf16());
    projection_from_units(units).unwrap_or_else(|| Utf16JsonProjection::from(""))
}

fn projection_from_units(units: Vec<u16>) -> Option<Utf16JsonProjection> {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units).ok()
}

#[cfg(test)]
mod native_workflow_projection_tests {
    use super::*;
    use command_api::{CommandFrontmatter, SlashCommandKind};

    #[test]
    fn workflow_kind_changes_description_but_is_not_an_xan_wire_field() {
        let command = SlashCommand {
            name: "saved-review".into(),
            description: "Run the saved review workflow".into(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            native_type: Some(NativeCommandType::Prompt),
            native_slash_kind: Some(NativeSlashKind::Workflow),
            kind: SlashCommandKind::Bundled {
                frontmatter: CommandFrontmatter::default(),
                prompt_fn: None,
            },
            ..SlashCommand::default()
        };
        let name = command.name.string_units("").expect("workflow name");
        let projected = project_command(&command, &name, &HashSet::new())
            .expect("workflow descriptor projects");
        assert_eq!(
            projected.string_units("/description"),
            Some(
                "Run the saved review workflow (dynamic workflow)"
                    .encode_utf16()
                    .collect()
            )
        );
        assert_eq!(projected.string_units("/kind"), None);
    }

    #[test]
    fn builtin_plugin_reservations_follow_both_entrypoint_gates() {
        let ordinary = native_builtin_plugin_spellings(None);
        assert!(ordinary.contains("plugin-authoring"));
        assert!(ordinary.contains("claude-test"));
        assert!(ordinary.contains("claude-test-execute"));
        assert!(ordinary.contains("claude-test-draft"));

        let remote = native_builtin_plugin_spellings(Some("remote_desktop"));
        assert!(remote.contains("plugin-authoring"));
        assert!(!remote.contains("claude-test"));

        let local_agent = native_builtin_plugin_spellings(Some("local-agent"));
        assert!(local_agent.is_empty());

        let native_facade = SlashCommand {
            name: "plugin-authoring".into(),
            source: CommandSource::Bundled,
            loaded_from: Some("bundled".into()),
            native_source: Some("built-in".into()),
            ..SlashCommand::default()
        };
        assert!(!native_builtin_plugin_descriptor_visible(
            &native_facade,
            Some("local-agent")
        ));

        let user_owned_same_name = SlashCommand {
            name: "plugin-authoring".into(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::User),
            loaded_from: Some("skills".into()),
            ..SlashCommand::default()
        };
        assert!(native_builtin_plugin_descriptor_visible(
            &user_owned_same_name,
            Some("local-agent")
        ));
    }

    #[test]
    fn reserved_set_uses_native_sources_and_descriptor_aliases_only() {
        let mut registry = CommandRegistry::new();
        let mut native = SlashCommand {
            name: "native-builtin".into(),
            description: "Native source row".into(),
            source: CommandSource::Builtin,
            native_catalog_order: Some(7),
            aliases: vec!["native-alias".into()],
            ..SlashCommand::default()
        };
        native.user_facing_name = Some("Native Display".into());
        registry.register_command(native);
        registry.register_alias("host-only-alias".into(), "native-builtin".into());
        registry.register_command(SlashCommand {
            name: "lingxi-only-bundled".into(),
            description: "Not in Native KCe input".into(),
            source: CommandSource::Bundled,
            ..SlashCommand::default()
        });
        registry.register_command(SlashCommand {
            name: "deep-research".into(),
            description: "Workflow producer descriptor".into(),
            source: CommandSource::Bundled,
            loaded_from: Some("bundled".into()),
            native_source: Some("built-in".into()),
            native_slash_kind: Some(NativeSlashKind::Workflow),
            ..SlashCommand::default()
        });

        let reserved = native_reserved_spellings(&registry, None);
        assert!(reserved.contains("native-builtin"));
        assert!(reserved.contains("native display"));
        assert!(reserved.contains("native-alias"));
        assert!(!reserved.contains("host-only-alias"));
        assert!(!reserved.contains("lingxi-only-bundled"));
        assert!(!reserved.contains("deep-research"));
    }
}
