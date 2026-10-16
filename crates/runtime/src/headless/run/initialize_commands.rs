//! Source-preserving command projection for the `-p` stream-json initialize
//! response. This follows Native `T5(Jr())`: the caller supplies the already
//! assembled non-interactive command pool, and this module applies only the
//! user-invocable and skill-Off projection before producing wire DTOs.

use command_api::{CommandRegistry, CommandSource, NativeCommandType, SlashCommand};
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
    let reserved_native_spellings = native_reserved_spellings(registry);
    let mut seen_names = HashSet::new();
    let mut projected = Vec::new();

    for command in registry.headless_init_commands() {
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

fn native_reserved_spellings(registry: &CommandRegistry) -> HashSet<String> {
    let mut reserved = HashSet::new();
    // Native KHt includes source-bound MZo descriptors and bundled skills.
    // The current registry exposes those as catalog-ordered rows and bundled
    // rows; its plugin metadata does not distinguish Native Fwr builtin-plugin
    // skills from installed plugins, so do not reserve arbitrary plugin names.
    // The emitted pool above remains the current headless_init_commands
    // projection.
    for command in registry.list_all() {
        if command.native_catalog_order.is_none() && command.source != CommandSource::Bundled {
            continue;
        }
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
        for alias in registry.aliases_for(&command.name) {
            if let Some(alias) = alias.string_units("") {
                insert_normalized_name(&mut reserved, &alias);
            }
        }
    }
    reserved
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
