//! `AgentSkillLoader` — the [`lingxi_core::host::skill_loader::SkillLoader`] impl that backs
//! G5 subagent skill preloading (claude `runAgent.ts:577-646`).
//!
//! claude's `runAgent` resolves each frontmatter `skills:` entry to a registered
//! command (`resolveSkillName`: exact match → the agent's plugin prefix
//! `pluginName:skill` → a `:skill` suffix match), loads its prompt
//! (`skill.getPromptForCommand('', ctx)`), and injects it as a meta user message.
//! This adapter ports that over the shared desktop [`CommandRegistry`] (the same
//! registry the slash dispatcher + `Skill` tool read), so a child agent runner
//! can preload skills WITHOUT the `agent` crate depending on `commands-core`
//! (the leaf [`lingxi_core::host::skill_loader`] seam breaks that cycle).
//!
//! The production composition root injects the same prompt-shell provider used
//! by the Skill tool and slash dispatcher, so preloads run the complete empty-
//! args pipeline: argument substitution, skill/session token replacement, then
//! policy-gated embedded `!command` expansion.

use std::sync::Arc;

use command_api::{CommandRegistry, SlashCommand, SlashCommandKind};
use lingxi_core::host::skill_loader::{SkillLoad, SkillLoader};
use lingxi_core::host::CancellationToken;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::ContentBlock;
use tokio::sync::RwLock;

fn projection_from_units(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("preloaded prompt is a root string")
}

fn replace_prompt_text(
    source: &Utf16JsonProjection,
    needle: &str,
    replacement: &str,
) -> Utf16JsonProjection {
    let source_units = source.string_units("").expect("prompt is a root string");
    let needle_units = needle.encode_utf16().collect::<Vec<_>>();
    let replacement_units = replacement.encode_utf16().collect::<Vec<_>>();
    if needle_units.is_empty() {
        return source.clone();
    }
    let mut output = Vec::with_capacity(source_units.len());
    let mut index = 0;
    while index < source_units.len() {
        if source_units[index..].starts_with(&needle_units) {
            output.extend_from_slice(&replacement_units);
            index += needle_units.len();
        } else {
            output.push(source_units[index]);
            index += 1;
        }
    }
    projection_from_units(output)
}

/// [`SkillLoader`] backed by the shared desktop [`CommandRegistry`].
pub struct AgentSkillLoader {
    registry: Arc<RwLock<CommandRegistry>>,
    /// Per-session id substituted for `${LINGXI_SESSION_ID}` in the skill body
    /// (claude `getSessionId()`); `None` leaves the token un-substituted.
    session_id: Option<String>,
    prompt_cwd: Option<Arc<tool_api::SessionCwd>>,
    /// Shared shell-expansion provider. `None` keeps hermetic tests free of
    /// process execution; production always wires the live provider.
    shell_expansion: Option<Arc<dyn command_api::ShellExpansionProvider>>,
}

impl AgentSkillLoader {
    /// Wrap the shared command-registry handle (the SAME `Arc` the slash
    /// dispatcher + `Skill` tool read), stamping `session_id` for
    /// `${LINGXI_SESSION_ID}` substitution.
    #[must_use]
    pub fn new(registry: Arc<RwLock<CommandRegistry>>, session_id: Option<String>) -> Self {
        Self {
            registry,
            session_id,
            prompt_cwd: None,
            shell_expansion: None,
        }
    }

    /// Supply the owning session's root and current directory.
    pub fn with_prompt_cwd(mut self, cwd: Arc<tool_api::SessionCwd>) -> Self {
        self.prompt_cwd = Some(cwd);
        self
    }

    /// Use the same policy-gated shell expansion as the Skill tool.
    #[must_use]
    pub fn with_shell_expansion(
        mut self,
        provider: Arc<dyn command_api::ShellExpansionProvider>,
    ) -> Self {
        self.shell_expansion = Some(provider);
        self
    }

    /// Resolve `skill_name` to a registered command name, mirroring claude
    /// `resolveSkillName` (runAgent.ts:945-973): (1) exact match (incl. aliases,
    /// via `resolve`), (2) the agent's plugin prefix `pluginName:skill`, (3) a
    /// `:skill` suffix match against any registered command.
    fn resolve_name(
        reg: &CommandRegistry,
        skill_name: &str,
        agent_type: &str,
    ) -> Option<Utf16JsonProjection> {
        // 1. Direct match (resolve follows aliases).
        let skill_name_source = Utf16JsonProjection::from(skill_name);
        if let Some(cmd) = reg.resolve(&skill_name_source) {
            return Some(cmd.name.clone());
        }
        // 2. Prefix with the agent's plugin name (agentType `"pluginName:agentName"`).
        if let Some(prefix) = agent_type.split(':').next() {
            if !prefix.is_empty() {
                let qualified = Utf16JsonProjection::from(format!("{prefix}:{skill_name}"));
                if let Some(cmd) = reg.resolve(&qualified) {
                    return Some(cmd.name.clone());
                }
            }
        }
        // 3. Suffix match — any command whose name ends with `:skillName`.
        let suffix = format!(":{skill_name}").encode_utf16().collect::<Vec<_>>();
        reg.list_all()
            .into_iter()
            .find(|c| {
                c.name
                    .string_units("")
                    .is_some_and(|units| units.ends_with(&suffix))
            })
            .map(|c| c.name.clone())
    }

    /// Build the skill's content blocks from a resolved markdown/plugin command.
    /// Returns `None` for non-prompt commands (builtin / MCP — claude's
    /// `skill.type !== 'prompt'` skip). Applies empty-args `$ARGUMENTS`/`$N`
    /// substitution, skill/session token replacement, and policy-gated
    /// embedded shell expansion.
    async fn to_skill_load(
        &self,
        cmd: &SlashCommand,
        display_name: &str,
        cwd_override: Option<&std::path::Path>,
        model: Option<&str>,
        cancellation: CancellationToken,
    ) -> Result<Option<SkillLoad>, String> {
        if cancellation.is_cancelled() {
            return Err("Agent skill preload cancelled".into());
        }
        let empty_args = Utf16JsonProjection::from("");
        let (frontmatter, prompt_template, dynamic) = match &cmd.kind {
            SlashCommandKind::Markdown {
                frontmatter,
                prompt_template,
                ..
            }
            | SlashCommandKind::Plugin {
                frontmatter,
                prompt_template,
                ..
            } => (frontmatter, prompt_template.clone(), false),
            // Bundled programmatic skill (`/loop`): prompt-typed, but the body is
            // produced by the dynamic builder with empty args (claude
            // `getPromptForCommand('', {isSkillPreload:true})`). Loading the
            // instructions must not activate a stopped loop. Session tokens apply.
            SlashCommandKind::Bundled {
                frontmatter,
                prompt_fn: Some(builder),
            } => {
                let cwd = cwd_override
                    .map(std::path::Path::to_path_buf)
                    .unwrap_or_else(|| {
                        self.prompt_cwd.as_ref().map_or_else(
                            || std::env::current_dir().unwrap_or_default(),
                            |state| state.cwd(),
                        )
                    });
                let root = self
                    .prompt_cwd
                    .as_ref()
                    .map_or_else(|| cwd.clone(), |state| state.project_root());
                let body = builder
                    .try_build_at(
                        &empty_args,
                        command_api::BundledPromptContext {
                            project_root: &root,
                            cwd: &cwd,
                            is_preload: true,
                            main_loop_model: model,
                            available_tool_names: None,
                            workflow_authoring_base_allowed: false,
                        },
                    )
                    .map_err(|error| error.to_string())?;
                (frontmatter, body, true)
            }
            // Builtin / MCP / inert-bundled commands are not prompt-based skills
            // (claude `skill.type !== 'prompt'`).
            SlashCommandKind::Builtin { .. }
            | SlashCommandKind::Mcp { .. }
            | SlashCommandKind::Bundled { .. } => return Ok(None),
        };
        // Empty-args argument substitution (claude `getPromptForCommand('', …)`;
        // matches the `Skill` tool's call: `Some(""), append=true`).
        let mut body = if dynamic {
            prompt_template
        } else {
            command_api::substitute_arguments_source(
                &prompt_template,
                &empty_args,
                true,
                &cmd.argument_names,
            )
            .unwrap_or_else(|_| prompt_template.clone())
        };
        if let Some(skill_root) = cmd.skill_root.as_ref() {
            let root = skill_root.to_string_lossy();
            let root = if cfg!(windows) {
                root.replace('\\', "/")
            } else {
                root.into_owned()
            };
            body = replace_prompt_text(&body, "${LINGXI_SKILL_DIR}", &root);
        }
        // `${LINGXI_SESSION_ID}` token (claude step 3, runs regardless of source).
        if let Some(sid) = &self.session_id {
            body = replace_prompt_text(&body, "${LINGXI_SESSION_ID}", sid);
        }
        if let Some(provider) = &self.shell_expansion {
            let shell_ctx = provider.build(
                frontmatter.allowed_tools.as_deref().unwrap_or(&[]),
                frontmatter.shell,
            );
            let slash_command_name = Utf16JsonProjection::from(format!("/{display_name}"));
            body = match command_api::execute_shell_commands_in_prompt(
                &body,
                &shell_ctx,
                &slash_command_name,
                frontmatter.shell,
                cancellation.clone(),
            )
            .await
            {
                Ok(expanded) => expanded,
                Err(error) => {
                    tracing::warn!(
                        skill = display_name,
                        error = %error,
                        "could not expand preloaded agent skill"
                    );
                    return Err(String::from_utf16_lossy(
                        &error
                            .source_message()
                            .string_units("")
                            .expect("shell expansion errors are root strings"),
                    ));
                }
            };
        }
        if cancellation.is_cancelled() {
            return Err("Agent skill preload cancelled".into());
        }
        let body_units = body
            .string_units("")
            .expect("preloaded skill prompt is a root string");
        Ok(Some(SkillLoad {
            display_name: display_name.into(),
            // The descriptor's frontmatter `model` is unrelated to progressMessage;
            // the command model carries no progressMessage in the Rust port, so
            // claude's default ("loading") applies — represented as `None`.
            progress_message: None,
            content: vec![ContentBlock::TextJsUtf16 {
                text: String::from_utf16_lossy(&body_units),
                utf16_code_units: body_units,
                citations: None,
            }],
        }))
    }
}

#[async_trait::async_trait]
impl SkillLoader for AgentSkillLoader {
    async fn resolve_and_load(
        &self,
        skill_name: &str,
        agent_type: &str,
        cwd: Option<&std::path::Path>,
        model: Option<&str>,
        cancellation: CancellationToken,
    ) -> Result<Option<SkillLoad>, String> {
        let reg = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err("Agent skill preload cancelled".into()),
            reg = self.registry.read() => reg,
        };
        let Some(resolved) = Self::resolve_name(&reg, skill_name, agent_type) else {
            return Ok(None);
        };
        let Some(cmd) = reg.resolve(&resolved).cloned() else {
            return Ok(None);
        };
        drop(reg);
        // claude passes the ORIGINAL `skillName` (the frontmatter entry) to
        // `formatSkillLoadingMetadata` (runAgent.ts:634), so the loading-metadata
        // block shows the name as authored, not the resolved/qualified name.
        self.to_skill_load(&cmd, skill_name, cwd, model, cancellation)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use command_api::{CommandFrontmatter, CommandSource};
    use std::path::PathBuf;

    fn md(name: &str, body: &str) -> SlashCommand {
        SlashCommand {
            name: name.to_string().into(),
            description: format!("{name} desc").into(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from(format!("/x/{name}.md")),
                frontmatter: CommandFrontmatter::default(),
                prompt_template: Utf16JsonProjection::from(body),
            },
            ..SlashCommand::default()
        }
    }

    #[tokio::test]
    async fn exact_match_loads_body_as_text_block() {
        let mut reg = CommandRegistry::new();
        reg.register_command(md("review", "REVIEW BODY"));
        let loader = AgentSkillLoader::new(Arc::new(RwLock::new(reg)), None);
        let load = loader
            .resolve_and_load(
                "review",
                "general-purpose",
                None,
                None,
                CancellationToken::new(),
            )
            .await
            .expect("checked skill preload")
            .expect("resolved");
        assert_eq!(load.display_name.string_at(""), Some("review"));
        assert_eq!(load.content.len(), 1);
        assert!(matches!(
            &load.content[0],
            ContentBlock::TextJsUtf16 { text, .. } if text == "REVIEW BODY"
        ));
    }

    #[tokio::test]
    async fn plugin_skill_preload_keeps_prompt_template_utf16_units() {
        let mut reg = CommandRegistry::new();
        let prompt_template =
            Utf16JsonProjection::parse(r#""Plugin body \ud800 stays exact.""#).unwrap();
        reg.register_command(SlashCommand {
            name: "source-plugin:greeter".into(),
            description: "greeter description".into(),
            source: CommandSource::Plugin,
            kind: SlashCommandKind::Plugin {
                plugin_id: lingxi_core::types::PluginId::new(),
                file_path: PathBuf::from("/x/source-plugin/greeter/SKILL.md"),
                frontmatter: CommandFrontmatter::default(),
                prompt_template,
            },
            loaded_from: Some("plugin".into()),
            ..SlashCommand::default()
        });
        let loader = AgentSkillLoader::new(Arc::new(RwLock::new(reg)), None);

        let load = loader
            .resolve_and_load(
                "source-plugin:greeter",
                "general-purpose",
                None,
                None,
                CancellationToken::new(),
            )
            .await
            .expect("checked plugin skill preload")
            .expect("plugin skill resolved");
        let mut expected = "Plugin body ".encode_utf16().collect::<Vec<_>>();
        expected.push(0xd800);
        expected.extend(" stays exact.".encode_utf16());
        assert!(matches!(
            &load.content[0],
            ContentBlock::TextJsUtf16 { utf16_code_units, .. }
                if utf16_code_units == &expected
        ));
    }

    #[tokio::test]
    async fn plugin_prefix_resolution() {
        // bare "feat" resolves via the agent's plugin prefix "pm:feat".
        let mut reg = CommandRegistry::new();
        reg.register_command(md("pm:feat", "FEATURE"));
        let loader = AgentSkillLoader::new(Arc::new(RwLock::new(reg)), None);
        let load = loader
            .resolve_and_load("feat", "pm:planner", None, None, CancellationToken::new())
            .await
            .expect("checked skill preload")
            .expect("resolved via plugin prefix");
        // display_name is the ORIGINAL frontmatter entry.
        assert_eq!(load.display_name.string_at(""), Some("feat"));
    }

    #[tokio::test]
    async fn suffix_match_resolution() {
        let mut reg = CommandRegistry::new();
        reg.register_command(md("some-plugin:deep", "DEEP"));
        let loader = AgentSkillLoader::new(Arc::new(RwLock::new(reg)), None);
        let load = loader
            .resolve_and_load("deep", "other-agent", None, None, CancellationToken::new())
            .await
            .expect("checked skill preload")
            .expect("resolved via suffix");
        assert_eq!(load.display_name.string_at(""), Some("deep"));
    }

    #[tokio::test]
    async fn unknown_skill_is_none() {
        let reg = CommandRegistry::new();
        let loader = AgentSkillLoader::new(Arc::new(RwLock::new(reg)), None);
        assert!(loader
            .resolve_and_load("nope", "a", None, None, CancellationToken::new())
            .await
            .expect("checked skill preload")
            .is_none());
    }

    #[tokio::test]
    async fn builtin_command_is_not_a_prompt_skill() {
        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "help".into(),
            description: "h".into(),
            source: CommandSource::Builtin,
            kind: SlashCommandKind::Builtin {
                handler_id: "help".to_string(),
            },
            ..SlashCommand::default()
        });
        let loader = AgentSkillLoader::new(Arc::new(RwLock::new(reg)), None);
        assert!(loader
            .resolve_and_load("help", "a", None, None, CancellationToken::new())
            .await
            .expect("checked skill preload")
            .is_none());
    }

    #[tokio::test]
    async fn session_id_token_substituted() {
        let mut reg = CommandRegistry::new();
        reg.register_command(md("s", "id=${LINGXI_SESSION_ID}"));
        let loader = AgentSkillLoader::new(Arc::new(RwLock::new(reg)), Some("sess:42".to_string()));
        let load = loader
            .resolve_and_load("s", "a", None, None, CancellationToken::new())
            .await
            .expect("checked skill preload")
            .expect("resolved");
        assert!(matches!(
            &load.content[0],
            ContentBlock::TextJsUtf16 { text, .. } if text == "id=sess:42"
        ));
    }
    #[tokio::test]
    async fn bundled_preload_uses_the_non_invoking_builder_entry() {
        struct PreloadOnly;
        impl command_api::BundledPromptFn for PreloadOnly {
            fn build(&self, _: &Utf16JsonProjection) -> Utf16JsonProjection {
                panic!("agent preload must not invoke the command");
            }
            fn build_for_preload(&self, args: &Utf16JsonProjection) -> Utf16JsonProjection {
                assert_eq!(args.string_at(""), Some(""));
                Utf16JsonProjection::from("preloaded instructions")
            }
        }
        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "loop".into(),
            kind: SlashCommandKind::Bundled {
                frontmatter: Default::default(),
                prompt_fn: Some(Arc::new(PreloadOnly)),
            },
            ..SlashCommand::default()
        });
        let loader = AgentSkillLoader::new(Arc::new(RwLock::new(reg)), None);
        let load = loader
            .resolve_and_load("loop", "a", None, None, CancellationToken::new())
            .await
            .expect("checked skill preload")
            .expect("resolved");
        assert!(
            matches!(&load.content[0], ContentBlock::TextJsUtf16 { text, .. } if text == "preloaded instructions")
        );
    }
    #[tokio::test]
    async fn checked_preload_propagates_error_with_root_and_live_cwd() {
        struct Checked;
        impl command_api::BundledPromptFn for Checked {
            fn build(&self, _: &Utf16JsonProjection) -> Utf16JsonProjection {
                panic!("unchecked preload");
            }
            fn try_build_at(
                &self,
                args: &Utf16JsonProjection,
                context: command_api::BundledPromptContext<'_>,
            ) -> std::io::Result<Utf16JsonProjection> {
                let root = context.project_root;
                let cwd = context.cwd;
                let preload = context.is_preload;
                assert_eq!(context.main_loop_model, Some("claude-fable-5"));
                assert_eq!(args.string_at(""), Some(""));
                assert!(preload);
                assert_eq!(root, std::path::Path::new("/project"));
                assert_eq!(cwd, std::path::Path::new("/project/subdir"));
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "loop.md denied",
                ))
            }
        }
        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "loop".into(),
            kind: SlashCommandKind::Bundled {
                frontmatter: Default::default(),
                prompt_fn: Some(Arc::new(Checked)),
            },
            ..Default::default()
        });
        let cwd = tool_api::SessionCwd::new("/project".into(), Vec::new());
        cwd.swap("/project/parent-dir".into(), Vec::new());
        let loader = AgentSkillLoader::new(Arc::new(RwLock::new(reg)), None).with_prompt_cwd(cwd);
        assert_eq!(
            loader
                .resolve_and_load(
                    "loop",
                    "agent",
                    Some(std::path::Path::new("/project/subdir")),
                    Some("claude-fable-5"),
                    CancellationToken::new()
                )
                .await
                .unwrap_err(),
            "loop.md denied"
        );
    }
}
