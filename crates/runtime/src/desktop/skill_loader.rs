//! `CommandRegistrySkillLoader` — bridges the `Skill` tool's [`SkillLoader`]
//! seam to the desktop [`CommandRegistry`].
//!
//! SKILLEXEC.2: the `Skill` tool resolves a model-supplied skill name to a real
//! slash command. TS does this via `getAllCommands()` + `findCommand`
//! (`tools/SkillTool/SkillTool.ts:399-409`); the Rust port injects this adapter
//! so `SkillTool::with_loader` reaches the same `CommandRegistry` the slash
//! dispatcher uses. The registry handle is shared (`Arc<RwLock<…>>`) with the
//! dispatcher, so plugin lifecycle mutations are visible to both.
//!
//! Mapping `SlashCommand` → [`SkillDescriptor`]:
//! - markdown / plugin-markdown commands are the model-invocable prompt skills
//!   (`command_type = Prompt`), carrying the markdown body as `body`, plus the
//!   frontmatter `model` / `allowed_tools` / `argument_names`;
//! - builtin and MCP-prompt commands map to `command_type = Other`, so the tool
//!   rejects them with the locked "is not a prompt-based skill" string (MCP
//!   prompt fetch has no Rust substrate — see the `SkillTool` module doc).

use std::sync::Arc;

use command_api::{CommandRegistry, SlashCommand, SlashCommandKind};
use tokio::sync::RwLock;
use tool_api::tool_trait::ToolError;
use tool_skill::skill::{SkillCommandType, SkillDescriptor, SkillLoader};

/// Project a registered [`SlashCommand`] onto the [`SkillDescriptor`] subset the
/// `Skill` tool surfaces. `session_id` is the engine's per-session id, stamped on
/// every descriptor so the `Skill` tool can substitute `${LINGXI_SESSION_ID}` in
/// the body (TS `getSessionId()`).
fn to_descriptor(cmd: &SlashCommand, session_id: Option<&str>) -> SkillDescriptor {
    let session_id = session_id.map(str::to_owned);
    match &cmd.kind {
        // Markdown-defined (project/user/managed) or plugin-shipped markdown
        // commands are the model-invocable prompt skills.
        SlashCommandKind::Markdown {
            frontmatter,
            prompt_template,
            ..
        }
        | SlashCommandKind::Plugin {
            frontmatter,
            prompt_template,
            ..
        } => SkillDescriptor {
            name: cmd.name.clone(),
            description: cmd.description.clone(),
            body: prompt_template.clone(),
            disable_model_invocation: cmd.disable_model_invocation,
            command_type: SkillCommandType::Prompt,
            model: frontmatter.model.clone(),
            effort: frontmatter.effort.clone(),
            allowed_tools: frontmatter.allowed_tools.clone().unwrap_or_default(),
            disallowed_tools: frontmatter.disallowed_tools.clone().unwrap_or_default(),
            argument_names: frontmatter.argument_names.clone(),
            // SKILLEXEC.6: forward the frontmatter `shell` selector so embedded
            // `!command` expansion routes through the author's chosen shell.
            shell: frontmatter.shell,
            // On-disk / plugin markdown is NOT MCP-sourced (TS `loadedFrom !==
            // 'mcp'`), so shell expansion runs for these commands.
            skip_shell_expansion: false,
            skill_root: cmd.skill_root.clone(),
            // Forked-skill declarations from the markdown frontmatter — these
            // are what make `context: fork` reach the Skill tool at all.
            context: frontmatter.context.clone(),
            background: frontmatter.background,
            agent: frontmatter.agent.clone(),
            session_id,
            dynamic_body: None,
        },
        // Builtin handlers are not prompt-based skills.
        SlashCommandKind::Builtin { .. } => SkillDescriptor {
            name: cmd.name.clone(),
            description: cmd.description.clone(),
            disable_model_invocation: cmd.disable_model_invocation,
            command_type: SkillCommandType::Other,
            session_id,
            ..SkillDescriptor::default()
        },
        // Bundled programmatic skills (the `/loop` family, port of
        // `registerBundledSkill`). Prompt-typed and model-invocable; the body is
        // produced dynamically by `prompt_fn` at call time (`getPromptForCommand`,
        // loop.ts:84), so `body` is left empty and `dynamic_body` carries the
        // builder.
        SlashCommandKind::Bundled {
            frontmatter,
            prompt_fn,
        } => SkillDescriptor {
            name: cmd.name.clone(),
            description: cmd.description.clone(),
            body: String::new(),
            disable_model_invocation: cmd.disable_model_invocation,
            command_type: SkillCommandType::Prompt,
            model: frontmatter.model.clone(),
            effort: frontmatter.effort.clone(),
            allowed_tools: frontmatter.allowed_tools.clone().unwrap_or_default(),
            disallowed_tools: frontmatter.disallowed_tools.clone().unwrap_or_default(),
            argument_names: Vec::new(),
            shell: frontmatter.shell,
            skip_shell_expansion: false,
            skill_root: None,
            context: frontmatter.context.clone(),
            background: frontmatter.background,
            agent: frontmatter.agent.clone(),
            session_id,
            dynamic_body: prompt_fn.clone(),
        },
        // MCP-prompt bridges are not prompt-based skills AND are remote/untrusted:
        // mark `skip_shell_expansion` so their body is never shell-expanded (TS
        // `loadedFrom !== 'mcp'` gate). Inert today since `Other` is rejected
        // before expansion, but kept faithful for when MCP prompts gain substrate.
        SlashCommandKind::Mcp { .. } => SkillDescriptor {
            name: cmd.name.clone(),
            description: cmd.description.clone(),
            disable_model_invocation: cmd.disable_model_invocation,
            command_type: SkillCommandType::Other,
            skip_shell_expansion: true,
            // MCP skills have no `baseDir` (TS), so `${LINGXI_SKILL_DIR}` is left
            // as-is; `${LINGXI_SESSION_ID}` is still substituted (TS step 3 runs
            // regardless of `loadedFrom`).
            session_id,
            ..SkillDescriptor::default()
        },
    }
}

/// [`SkillLoader`] backed by the shared desktop [`CommandRegistry`].
pub struct CommandRegistrySkillLoader {
    registry: Arc<RwLock<CommandRegistry>>,
    /// The engine's per-session id, stamped on every resolved descriptor so the
    /// `Skill` tool substitutes `${LINGXI_SESSION_ID}` in the body (TS
    /// `getSessionId()`). `None` keeps the token un-substituted (e.g. tests that
    /// construct the loader without a session).
    session_id: Option<String>,
    invocation_observer: Option<command_api::SkillInvocationObserver>,
}

impl CommandRegistrySkillLoader {
    /// Wrap the shared command registry handle. The same `Arc` is later filled
    /// with the fully-built registry and handed to the slash dispatcher, so the
    /// loader and dispatcher observe one command set. Constructs with no session
    /// id (the `${LINGXI_SESSION_ID}` token is left as-is); use
    /// [`Self::with_session_id`] to wire the engine's session id.
    #[must_use]
    pub fn new(registry: Arc<RwLock<CommandRegistry>>) -> Self {
        Self {
            registry,
            session_id: None,
            invocation_observer: None,
        }
    }

    /// Wrap the shared command registry handle and stamp `session_id` on every
    /// resolved descriptor (so `${LINGXI_SESSION_ID}` substitutes to it).
    #[must_use]
    pub fn with_session_id(registry: Arc<RwLock<CommandRegistry>>, session_id: String) -> Self {
        Self {
            session_id: Some(session_id),
            ..Self::new(registry)
        }
    }

    /// Wire the plugin-monitor observer used after a prompt descriptor passes
    /// the Skill tool's invocation guards.
    #[must_use]
    pub fn with_invocation_observer(
        mut self,
        observer: command_api::SkillInvocationObserver,
    ) -> Self {
        self.invocation_observer = Some(observer);
        self
    }
}

#[async_trait::async_trait]
impl SkillLoader for CommandRegistrySkillLoader {
    async fn load(&self, name: &str) -> Result<Option<SkillDescriptor>, ToolError> {
        // `resolve` follows aliases (TS `findCommand` over name + aliases).
        let reg = self.registry.read().await;
        Ok(reg
            .resolve(name)
            .filter(|cmd| reg.session_skill_allowed(cmd))
            .map(|cmd| to_descriptor(cmd, self.session_id.as_deref())))
    }

    async fn skill_invoked(&self, name: &str) {
        if let Some(observer) = self.invocation_observer.as_ref() {
            observer(name.to_string()).await;
        }
    }

    /// Every registered command name, so a lookup miss on a bare `foo` can
    /// point at a plugin-qualified `some-plugin:foo`. Only names are exposed —
    /// the suggestion never needs a descriptor, and building them all would
    /// touch disk for a path that is already an error.
    async fn list_names(&self) -> Vec<String> {
        let reg = self.registry.read().await;
        reg.list_all()
            .iter()
            .filter(|cmd| reg.session_skill_allowed(cmd))
            .map(|cmd| cmd.name.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use command_api::{CommandFrontmatter, CommandSource};
    use std::path::PathBuf;

    fn markdown_cmd(name: &str, body: &str) -> SlashCommand {
        SlashCommand {
            name: name.to_string(),
            description: format!("{name} description"),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            kind: SlashCommandKind::Markdown {
                file_path: PathBuf::from(format!("/x/{name}.md")),
                frontmatter: CommandFrontmatter {
                    model: Some("opus".to_string()),
                    allowed_tools: Some(vec!["Bash".to_string()]),
                    argument_names: vec!["name".to_string()],
                    ..CommandFrontmatter::default()
                },
                prompt_template: body.to_string(),
            },
            ..SlashCommand::default()
        }
    }

    #[tokio::test]
    async fn resolves_registered_markdown_command_to_prompt_descriptor() {
        let mut reg = CommandRegistry::new();
        reg.register_command(markdown_cmd("review-pr", "Review $ARGUMENTS"));
        let loader = CommandRegistrySkillLoader::new(Arc::new(RwLock::new(reg)));

        let desc = loader
            .load("review-pr")
            .await
            .expect("load ok")
            .expect("command present");
        assert_eq!(desc.name, "review-pr");
        assert_eq!(desc.description, "review-pr description");
        assert_eq!(desc.body, "Review $ARGUMENTS");
        assert_eq!(desc.command_type, SkillCommandType::Prompt);
        assert_eq!(desc.model.as_deref(), Some("opus"));
        assert_eq!(desc.allowed_tools, vec!["Bash".to_string()]);
        assert_eq!(desc.argument_names, vec!["name".to_string()]);
        assert!(!desc.disable_model_invocation);
    }

    #[tokio::test]
    async fn session_skill_grants_filter_load_and_names_without_hiding_manual_commands() {
        let mut registry = CommandRegistry::new();
        registry.register_command(markdown_cmd("verify", "body"));
        registry.register_command(markdown_cmd("simplify", "body"));
        registry.set_session_skill_allowlist(Some(vec!["verify".to_string()]));
        let registry = Arc::new(RwLock::new(registry));
        let loader = CommandRegistrySkillLoader::new(registry.clone());
        assert!(loader.load("verify").await.unwrap().is_some());
        assert!(loader.load("simplify").await.unwrap().is_none());
        assert_eq!(loader.list_names().await, vec!["verify"]);
        assert!(registry.read().await.resolve("simplify").is_some());
        registry.write().await.set_session_skill_allowlist(None);
        assert!(loader.load("simplify").await.unwrap().is_some());
    }

    /// A skill's declared `effort` must reach the descriptor.
    ///
    /// It previously stopped at the frontmatter reader, which never parsed the
    /// key, so `try_fork` fed `effort: None` into every forked-skill scoping
    /// record no matter what the skill declared. Upstream's skill->command
    /// builder carries it (`wXe({… effort: De …})`) and the scoping spreads it
    /// in conditionally (`…n.effort !== void 0 && { effort: n.effort }`).
    #[tokio::test]
    async fn a_declared_effort_reaches_the_descriptor() {
        let mut reg = CommandRegistry::new();
        let mut cmd = markdown_cmd("deep-review", "body");
        if let SlashCommandKind::Markdown { frontmatter, .. } = &mut cmd.kind {
            frontmatter.effort = Some("high".to_string());
        }
        reg.register_command(cmd);
        reg.register_command(markdown_cmd("plain", "body"));
        let loader = CommandRegistrySkillLoader::new(Arc::new(RwLock::new(reg)));

        let declared = loader
            .load("deep-review")
            .await
            .expect("load ok")
            .expect("present");
        assert_eq!(
            declared.effort.as_deref(),
            Some("high"),
            "a declared effort must survive to the descriptor"
        );

        let undeclared = loader
            .load("plain")
            .await
            .expect("load ok")
            .expect("present");
        assert_eq!(
            undeclared.effort, None,
            "a skill that declares none stays none"
        );
    }

    #[tokio::test]
    async fn unregistered_name_resolves_to_none() {
        let reg = CommandRegistry::new();
        let loader = CommandRegistrySkillLoader::new(Arc::new(RwLock::new(reg)));
        assert!(loader.load("nope").await.expect("load ok").is_none());
    }

    #[tokio::test]
    async fn builtin_command_maps_to_non_prompt_other() {
        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "help".to_string(),
            description: "Show help".to_string(),
            source: CommandSource::Builtin,
            kind: SlashCommandKind::Builtin {
                handler_id: "help".to_string(),
            },
            ..SlashCommand::default()
        });
        let loader = CommandRegistrySkillLoader::new(Arc::new(RwLock::new(reg)));
        let desc = loader
            .load("help")
            .await
            .expect("load ok")
            .expect("present");
        // → rejected by the tool with the locked "is not a prompt-based skill".
        assert_eq!(desc.command_type, SkillCommandType::Other);
    }

    #[tokio::test]
    async fn ordinary_markdown_command_does_not_carry_skill_root() {
        let mut reg = CommandRegistry::new();
        reg.register_command(markdown_cmd("dir-skill", "uses ${LINGXI_SKILL_DIR}"));
        let loader = CommandRegistrySkillLoader::with_session_id(
            Arc::new(RwLock::new(reg)),
            "sess:test-1".to_string(),
        );
        let desc = loader
            .load("dir-skill")
            .await
            .expect("load ok")
            .expect("present");
        assert!(desc.skill_root.is_none());
        assert_eq!(desc.session_id.as_deref(), Some("sess:test-1"));
    }

    #[tokio::test]
    async fn skills_loaded_command_carries_declared_skill_root() {
        let mut cmd = markdown_cmd("dir-skill", "uses ${LINGXI_SKILL_DIR}");
        cmd.loaded_from = Some("skills".to_string());
        cmd.skill_root = Some(PathBuf::from("/x/dir-skill"));
        let mut reg = CommandRegistry::new();
        reg.register_command(cmd);
        let loader = CommandRegistrySkillLoader::new(Arc::new(RwLock::new(reg)));
        let desc = loader
            .load("dir-skill")
            .await
            .expect("load ok")
            .expect("present");
        assert_eq!(
            desc.skill_root.as_deref(),
            Some(std::path::Path::new("/x/dir-skill"))
        );
        assert!(!desc.skip_shell_expansion);
    }

    #[tokio::test]
    async fn no_session_id_leaves_descriptor_session_unset() {
        let mut reg = CommandRegistry::new();
        reg.register_command(markdown_cmd("plain", "body"));
        // `new` wires no session id.
        let loader = CommandRegistrySkillLoader::new(Arc::new(RwLock::new(reg)));
        let desc = loader
            .load("plain")
            .await
            .expect("load ok")
            .expect("present");
        assert!(desc.session_id.is_none());
        assert!(desc.skill_root.is_none());
    }

    #[tokio::test]
    async fn bundled_command_maps_to_prompt_with_dynamic_body() {
        use command_api::BundledPromptFn;
        struct B;
        impl BundledPromptFn for B {
            fn build(&self, args: &str) -> String {
                format!("built:{args}")
            }
        }
        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "loop".to_string(),
            description: "loopy".to_string(),
            source: CommandSource::Bundled,
            kind: SlashCommandKind::Bundled {
                frontmatter: CommandFrontmatter {
                    model: Some("opus".to_string()),
                    allowed_tools: Some(vec!["CronCreate".to_string()]),
                    ..CommandFrontmatter::default()
                },
                prompt_fn: Some(Arc::new(B)),
            },
            loaded_from: Some("bundled".to_string()),
            ..SlashCommand::default()
        });
        let loader = CommandRegistrySkillLoader::new(Arc::new(RwLock::new(reg)));
        let desc = loader
            .load("loop")
            .await
            .expect("load ok")
            .expect("present");
        assert_eq!(desc.command_type, SkillCommandType::Prompt);
        assert_eq!(desc.description, "loopy");
        assert_eq!(desc.model.as_deref(), Some("opus"));
        assert_eq!(desc.allowed_tools, vec!["CronCreate".to_string()]);
        let builder = desc.dynamic_body.as_ref().expect("dynamic_body carried");
        assert_eq!(builder.build("x"), "built:x");
    }

    #[tokio::test]
    async fn disable_model_invocation_flag_is_carried() {
        let mut reg = CommandRegistry::new();
        let mut cmd = markdown_cmd("internal", "body");
        cmd.disable_model_invocation = true;
        reg.register_command(cmd);
        let loader = CommandRegistrySkillLoader::new(Arc::new(RwLock::new(reg)));
        let desc = loader
            .load("internal")
            .await
            .expect("load ok")
            .expect("present");
        assert!(desc.disable_model_invocation);
        assert_eq!(desc.command_type, SkillCommandType::Prompt);
    }

    /// The suggestion in `Unknown skill:` can only name a plugin-qualified
    /// command if the loader can enumerate one. The trait's default returns an
    /// empty list — correct for hermetic loaders, but silently disabling the
    /// hint here — so pin that THIS loader overrides it.
    #[tokio::test]
    async fn list_names_reports_the_registered_commands() {
        let mut reg = CommandRegistry::new();
        reg.register_command(markdown_cmd("some-plugin:deploy", "body"));
        reg.register_command(markdown_cmd("other", "body"));
        let loader = CommandRegistrySkillLoader::new(Arc::new(RwLock::new(reg)));
        let mut names = loader.list_names().await;
        names.sort();
        assert_eq!(names, vec!["other", "some-plugin:deploy"]);
    }
}
