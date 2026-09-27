//! MCP prompts as slash commands — claude-code's `getAllCommands` merge.
//!
//! An MCP server can advertise PROMPTS alongside its tools. Claude folds them
//! into the slash-command list, which is what makes one reachable as
//! `/<server>:<prompt>` and findable by the `Skill` tool. The port has always
//! fetched them (`prompts/list` at connect, refreshed on
//! `notifications/prompts/list_changed`) and cached them on the connection —
//! nothing read them back, so they existed on the wire and nowhere else.
//!
//! Naming is `<server>:<prompt>`, matching the binary's `${server}:` prefix
//! test when it filters the merged list back down per server.
//!
//! These commands are deliberately NOT shell-expanded and NOT forkable: an MCP
//! prompt's body is fetched from a remote server, so it is untrusted input, and
//! `SkillTool` already refuses to expand `SlashCommandKind::Mcp`.

use protocol::McpConnectionId;

use crate::model::{CommandSource, SlashCommand, SlashCommandKind};

/// The `loaded_from` marker claude uses for an MCP-sourced command.
pub const MCP_LOADED_FROM: &str = "mcp";

/// Build the `<server>:<prompt>` slash-command name.
#[must_use]
pub fn mcp_prompt_command_name(server: &str, prompt: &str) -> String {
    format!("{server}:{prompt}")
}

/// Fold one server's advertised prompts into slash commands.
///
/// `description` falls back to the command name when the server supplied none —
/// an empty description would render a blank row in the command picker.
/// `has_user_specified_description` tracks whether the server actually supplied
/// one, mirroring the same flag on markdown commands.
#[must_use]
pub fn mcp_prompt_commands(
    prompts: &[(String, McpConnectionId, platform_api::McpPromptDto)],
) -> Vec<SlashCommand> {
    prompts
        .iter()
        .map(|(server, connection_id, prompt)| {
            let name = mcp_prompt_command_name(server, &prompt.name);
            let described = prompt
                .description
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty());
            SlashCommand {
                description: described.unwrap_or(&name).to_string(),
                has_user_specified_description: described.is_some(),
                name,
                source: CommandSource::Mcp,
                kind: SlashCommandKind::Mcp {
                    connection_id: *connection_id,
                    prompt_name: prompt.name.clone(),
                    arguments: prompt.arguments.clone(),
                },
                argument_hint: (!prompt.arguments.is_empty()).then(|| {
                    prompt
                        .arguments
                        .iter()
                        .map(|argument| {
                            if argument.required {
                                format!("<{}>", argument.name)
                            } else {
                                format!("[{}]", argument.name)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(
        server: &str,
        name: &str,
        description: Option<&str>,
    ) -> (String, McpConnectionId, platform_api::McpPromptDto) {
        (
            server.to_string(),
            McpConnectionId::new(),
            platform_api::McpPromptDto {
                name: name.to_string(),
                description: description.map(str::to_string),
                arguments: Vec::new(),
            },
        )
    }

    #[test]
    fn a_prompt_becomes_a_server_qualified_command() {
        let cmds = mcp_prompt_commands(&[prompt("github", "review_pr", Some("Review a PR"))]);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "github:review_pr");
        assert_eq!(cmds[0].description, "Review a PR");
        assert!(cmds[0].has_user_specified_description);
        assert_eq!(cmds[0].loaded_from.as_deref(), Some("mcp"));
        assert!(matches!(cmds[0].kind, SlashCommandKind::Mcp { .. }));
    }

    /// A server that supplied no description must not render a blank row.
    #[test]
    fn a_description_less_prompt_falls_back_to_its_name() {
        for desc in [None, Some(""), Some("   ")] {
            let cmds = mcp_prompt_commands(&[prompt("gh", "p", desc)]);
            assert_eq!(cmds[0].description, "gh:p", "{desc:?}");
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
        assert_eq!(cmds[0].name, "a:summarize");
        assert_eq!(cmds[1].name, "b:summarize");
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
    fn no_prompts_yields_no_commands() {
        assert!(mcp_prompt_commands(&[]).is_empty());
    }

    #[test]
    fn prompt_arguments_drive_hint_and_positional_names() {
        let mut advertised = prompt("github", "review", Some("Review"));
        advertised.2.arguments = vec![
            platform_api::McpPromptArgumentDto {
                name: "owner".to_string(),
                description: Some("Repository owner".to_string()),
                required: true,
            },
            platform_api::McpPromptArgumentDto {
                name: "focus".to_string(),
                description: None,
                required: false,
            },
        ];
        let commands = mcp_prompt_commands(&[advertised]);
        assert_eq!(commands[0].source, CommandSource::Mcp);
        assert_eq!(commands[0].argument_names, ["owner", "focus"]);
        assert_eq!(
            commands[0].argument_hint.as_deref(),
            Some("<owner> [focus]")
        );
    }
}
