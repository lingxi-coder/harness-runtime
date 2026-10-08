//! MCP prompts as slash commands — claude-code's `getAllCommands` merge.
//!
//! An MCP server can advertise PROMPTS alongside its tools. Claude folds them
//! into the slash-command list, which is what makes one reachable as
//! `/<server>:<prompt>` and findable by the `Skill` tool. The port has always
//! fetched them (`prompts/list` at connect, refreshed on
//! `notifications/prompts/list_changed`) and cached them on the connection —
//! nothing read them back, so they existed on the wire and nowhere else.
//!
//! Native gives each prompt a stable `mcp__<sanitized-server>__<prompt>` name
//! and the two aliases `<server>:<prompt>` and `<server>:<prompt> (MCP)`. The
//! aliases are important for both user dispatch and server-scoped filtering.
//!
//! These commands are deliberately NOT shell-expanded and NOT forkable: an MCP
//! prompt's body is fetched from a remote server, so it is untrusted input, and
//! `SkillTool` already refuses to expand `SlashCommandKind::Mcp`.

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::McpConnectionId;

use crate::model::{CommandSource, SlashCommand, SlashCommandKind};

/// The `loaded_from` marker claude uses for an MCP-sourced command.
pub const MCP_LOADED_FROM: &str = "mcp";

/// Build Native's `mcp__<sanitized-server>__<prompt>` slash-command name.
#[must_use]
pub fn mcp_prompt_command_name(server: &str, prompt: &Utf16JsonProjection) -> Utf16JsonProjection {
    let mut units = "mcp__".encode_utf16().collect::<Vec<_>>();
    units.extend(server.encode_utf16().map(sanitize_server_unit));
    units.extend("__".encode_utf16());
    units.extend(prompt.string_units("").unwrap_or_default());
    let display = String::from_utf16_lossy(&units);
    Utf16JsonProjection::root_string(display, units)
        .expect("display projection is derived from the same UTF-16 units")
}

/// Build Native's exact and `(MCP)` aliases for a server prompt.
#[must_use]
pub fn mcp_prompt_command_aliases(
    server: &str,
    prompt: &Utf16JsonProjection,
) -> Vec<Utf16JsonProjection> {
    let mut base = server.encode_utf16().collect::<Vec<_>>();
    base.push(b':' as u16);
    base.extend(prompt.string_units("").unwrap_or_default());
    let mut marked = base.clone();
    marked.extend(" (MCP)".encode_utf16());
    vec![projection_from_units(base), projection_from_units(marked)]
}

fn sanitize_server_unit(unit: u16) -> u16 {
    if (b'a' as u16..=b'z' as u16).contains(&unit)
        || (b'A' as u16..=b'Z' as u16).contains(&unit)
        || (b'0' as u16..=b'9' as u16).contains(&unit)
        || unit == b'_' as u16
        || unit == b'-' as u16
    {
        unit
    } else {
        b'_' as u16
    }
}

fn projection_from_units(units: Vec<u16>) -> Utf16JsonProjection {
    let display = String::from_utf16_lossy(&units);
    Utf16JsonProjection::root_string(display, units)
        .expect("display projection is derived from the same UTF-16 units")
}

fn trim_projection(value: &Utf16JsonProjection) -> Utf16JsonProjection {
    let units = value.string_units("").unwrap_or_default();
    let is_whitespace = |unit: &u16| {
        char::from_u32(u32::from(*unit))
            .is_some_and(lingxi_core::host::effort::javascript_whitespace)
    };
    let start = units
        .iter()
        .position(|unit| !is_whitespace(unit))
        .unwrap_or(units.len());
    let end = units
        .iter()
        .rposition(|unit| !is_whitespace(unit))
        .map_or(start, |index| index + 1);
    let trimmed = units[start..end].to_vec();
    let display = String::from_utf16_lossy(&trimmed);
    Utf16JsonProjection::root_string(display, trimmed)
        .expect("display projection is derived from the same UTF-16 units")
}

/// Fold one server's advertised prompts into slash commands.
///
/// `description` falls back to the command name when the server supplied none —
/// an empty description would render a blank row in the command picker.
/// `has_user_specified_description` tracks whether the server actually supplied
/// one, mirroring the same flag on markdown commands.
#[must_use]
pub fn mcp_prompt_commands(
    prompts: &[(String, McpConnectionId, lingxi_core::host::McpPromptDto)],
) -> Vec<SlashCommand> {
    prompts
        .iter()
        .map(|(server, connection_id, prompt)| {
            let name = mcp_prompt_command_name(server, &prompt.name);
            let described =
                prompt
                    .description
                    .as_ref()
                    .map(trim_projection)
                    .filter(|description| {
                        description
                            .string_units("")
                            .is_some_and(|units| !units.is_empty())
                    });
            SlashCommand {
                description: described.clone().unwrap_or_else(|| name.clone()),
                has_user_specified_description: described.is_some(),
                name,
                source: CommandSource::Mcp,
                aliases: mcp_prompt_command_aliases(server, &prompt.name),
                kind: SlashCommandKind::Mcp {
                    connection_id: *connection_id,
                    prompt_name: prompt.name.clone(),
                    arguments: prompt.arguments.clone(),
                },
                argument_hint: (!prompt.arguments.is_empty()).then(|| {
                    let mut units = Vec::new();
                    for (index, argument) in prompt.arguments.iter().enumerate() {
                        if index > 0 {
                            units.push(b' ' as u16);
                        }
                        let bracketed = if argument.required {
                            bracket_argument_name(&argument.name, b'<', b'>')
                        } else {
                            bracket_argument_name(&argument.name, b'[', b']')
                        };
                        units.extend(bracketed.string_units("").unwrap_or_default());
                    }
                    let display = String::from_utf16_lossy(&units);
                    Utf16JsonProjection::root_string(display, units)
                        .expect("display projection is derived from the same UTF-16 units")
                }),
                argument_names: prompt
                    .arguments
                    .iter()
                    .map(|argument| argument.name.clone())
                    .collect(),
                loaded_from: Some(MCP_LOADED_FROM.to_string()),
                ..SlashCommand::default()
            }
        })
        .collect()
}

fn bracket_argument_name(name: &Utf16JsonProjection, open: u8, close: u8) -> Utf16JsonProjection {
    let mut units = vec![u16::from(open)];
    units.extend(name.string_units("").unwrap_or_default());
    units.push(u16::from(close));
    let display = String::from_utf16_lossy(&units);
    Utf16JsonProjection::root_string(display, units)
        .expect("display projection is derived from the same UTF-16 units")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(
        server: &str,
        name: &str,
        description: Option<&str>,
    ) -> (String, McpConnectionId, lingxi_core::host::McpPromptDto) {
        (
            server.to_string(),
            McpConnectionId::new(),
            lingxi_core::host::McpPromptDto {
                name: name.into(),
                description: description.map(Into::into),
                arguments: Vec::new(),
            },
        )
    }

    #[test]
    fn a_prompt_becomes_a_server_qualified_command() {
        let cmds = mcp_prompt_commands(&[prompt("github", "review_pr", Some("Review a PR"))]);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name.string_at(""), Some("mcp__github__review_pr"));
        assert_eq!(
            cmds[0]
                .aliases
                .iter()
                .map(|alias| alias.string_at(""))
                .collect::<Vec<_>>(),
            [Some("github:review_pr"), Some("github:review_pr (MCP)")]
        );
        assert_eq!(cmds[0].description.string_at(""), Some("Review a PR"));
        assert!(cmds[0].has_user_specified_description);
        assert_eq!(cmds[0].loaded_from.as_deref(), Some("mcp"));
        assert!(matches!(cmds[0].kind, SlashCommandKind::Mcp { .. }));
    }

    /// A server that supplied no description must not render a blank row.
    #[test]
    fn a_description_less_prompt_falls_back_to_its_name() {
        for desc in [None, Some(""), Some("   ")] {
            let cmds = mcp_prompt_commands(&[prompt("gh", "p", desc)]);
            assert_eq!(
                cmds[0].description.string_at(""),
                Some("mcp__gh__p"),
                "{desc:?}"
            );
            assert!(
                !cmds[0].has_user_specified_description,
                "an absent/blank description is not user-specified: {desc:?}"
            );
        }
    }

    /// The connection id rides onto the command so dispatch can `prompts/get`
    /// from the RIGHT server when two servers advertise the same prompt name.
    #[test]
    fn same_named_prompts_on_two_servers_stay_distinct() {
        let cmds = mcp_prompt_commands(&[
            prompt("a", "summarize", None),
            prompt("b", "summarize", None),
        ]);
        assert_eq!(cmds[0].name.string_at(""), Some("mcp__a__summarize"));
        assert_eq!(cmds[1].name.string_at(""), Some("mcp__b__summarize"));
        assert_eq!(cmds[0].aliases[0].string_at(""), Some("a:summarize"));
        assert_eq!(cmds[1].aliases[0].string_at(""), Some("b:summarize"));
        let ids: Vec<_> = cmds
            .iter()
            .map(|c| match &c.kind {
                SlashCommandKind::Mcp { connection_id, .. } => *connection_id,
                _ => panic!("expected Mcp"),
            })
            .collect();
        assert_ne!(ids[0], ids[1], "each command keeps its own connection");
    }

    #[test]
    fn server_name_uses_native_ascii_sanitization_per_utf16_unit() {
        let command = mcp_prompt_commands(&[prompt("acme🙂", "review", None)])
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(command.name.string_at(""), Some("mcp__acme____review"));
        assert_eq!(command.aliases[0].string_at(""), Some("acme🙂:review"));
    }

    #[test]
    fn prompt_names_descriptions_and_argument_labels_keep_lone_surrogates() {
        let mut source = prompt("mcp", "safe", None);
        source.2.name =
            Utf16JsonProjection::root_string("p�".into(), vec![b'p' as u16, 0xd800]).unwrap();
        source.2.description =
            Some(Utf16JsonProjection::root_string("d�".into(), vec![b'd' as u16, 0xd800]).unwrap());
        source.2.arguments = vec![lingxi_core::host::McpPromptArgumentDto {
            name: Utf16JsonProjection::root_string("a�".into(), vec![b'a' as u16, 0xdfff]).unwrap(),
            description: None,
            required: true,
        }];

        let commands = mcp_prompt_commands(&[source]);
        assert_eq!(
            commands[0].name.string_units(""),
            Some(
                [
                    "mcp__mcp__".encode_utf16().collect::<Vec<_>>(),
                    vec![b'p' as u16, 0xd800]
                ]
                .concat()
            )
        );
        assert_eq!(
            commands[0].aliases[0].string_units(""),
            Some(
                [
                    "mcp:".encode_utf16().collect::<Vec<_>>(),
                    vec![b'p' as u16, 0xd800]
                ]
                .concat()
            )
        );
        assert_eq!(
            commands[0].description.as_ref().unwrap().string_units(""),
            Some(vec![b'd' as u16, 0xd800])
        );
        assert_eq!(
            commands[0].argument_names[0].string_units(""),
            Some(vec![b'a' as u16, 0xdfff])
        );
        assert_eq!(
            commands[0].argument_hint.as_ref().unwrap().string_units(""),
            Some(vec![b'<' as u16, b'a' as u16, 0xdfff, b'>' as u16])
        );

        let wire = serde_json::to_string(&commands[0]).unwrap();
        assert!(wire.contains("\\ud800"));
        assert!(wire.contains("\\udfff"));
        let decoded: SlashCommand = serde_json::from_str(&wire).unwrap();
        assert_eq!(
            decoded.name.string_units(""),
            commands[0].name.string_units("")
        );
        assert_eq!(
            decoded.description.unwrap().string_units(""),
            commands[0].description.as_ref().unwrap().string_units("")
        );
    }

    #[test]
    fn no_prompts_yields_no_commands() {
        assert!(mcp_prompt_commands(&[]).is_empty());
    }

    #[test]
    fn prompt_arguments_drive_hint_and_positional_names() {
        let mut advertised = prompt("github", "review", Some("Review"));
        advertised.2.arguments = vec![
            lingxi_core::host::McpPromptArgumentDto {
                name: "owner".into(),
                description: Some("Repository owner".into()),
                required: true,
            },
            lingxi_core::host::McpPromptArgumentDto {
                name: "focus".into(),
                description: None,
                required: false,
            },
        ];
        let commands = mcp_prompt_commands(&[advertised]);
        assert_eq!(commands[0].source, CommandSource::Mcp);
        assert_eq!(
            commands[0]
                .argument_names
                .iter()
                .map(|value| value.string_at(""))
                .collect::<Vec<_>>(),
            [Some("owner"), Some("focus")]
        );
        assert_eq!(
            commands[0]
                .argument_hint
                .as_ref()
                .and_then(|value| value.string_at("")),
            Some("<owner> [focus]")
        );
    }
}
