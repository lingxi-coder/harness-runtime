//! Live-registry `Skill`-tool loader for the mobile engine.
//!
//! Mobile now shares ONE `Arc<RwLock<CommandRegistry>>` across the slash
//! dispatcher, skill listing provider, and the `Skill` tool. That keeps bundled
//! skills, programmatic bundled prompts such as `/loop`, and on-disk
//! `.lingxi/commands/**.md` / `.lingxi/skills/<name>/SKILL.md` in one live
//! command set, so boot-time loading and later `/reload-skills` refreshes are
//! visible everywhere instead of freezing the `Skill` tool at construction.
//!
//! The `to_descriptor` projection stays byte-faithful to the desktop
//! `CommandRegistrySkillLoader`, so a prompt command resolves identically on both
//! platforms.

use std::path::Path;
use std::sync::Arc;

use command_api::{CommandRegistry, CommandSource, SlashCommand, SlashCommandKind};
use lingxi_core::host::skill_loader::{SkillLoad, SkillLoader as AgentSkillLoader};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::ContentBlock;
use session::jsonl::SessionMode;
use tokio::sync::RwLock;
use tool_api::tool_trait::ToolError;
use tool_skill::skill::{SkillCommandType, SkillDescriptor, SkillLoader as ToolSkillLoader};

fn namespaced_name(prefix: &str, name: &Utf16JsonProjection) -> Utf16JsonProjection {
    let mut units = prefix.encode_utf16().collect::<Vec<_>>();
    units.extend(
        name.string_units("")
            .expect("registered command name is a root string"),
    );
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
        .expect("namespaced command name is a root string")
}

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

/// Project a registered [`SlashCommand`] onto the [`SkillDescriptor`] subset the
/// `Skill` tool surfaces (verbatim with harness-runtime::desktop's `skill_loader::to_descriptor`).
/// `session_id` is stamped on every descriptor so the tool can substitute
/// `${LINGXI_SESSION_ID}` in the body.
fn to_descriptor(cmd: &SlashCommand, session_id: Option<&str>) -> SkillDescriptor {
    let session_id = session_id.map(str::to_owned);
    let mut descriptor = match &cmd.kind {
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
            argument_names: cmd.argument_names.clone(),
            shell: frontmatter.shell,
            skip_shell_expansion: false,
            skill_root: cmd.skill_root.clone(),
            context: frontmatter.context.clone(),
            background: frontmatter.background,
            agent: frontmatter.agent.clone(),
            session_id,
            dynamic_body: None,
        },
        SlashCommandKind::Bundled {
            frontmatter,
            prompt_fn,
        } => SkillDescriptor {
            name: cmd.name.clone(),
            description: cmd.description.clone(),
            body: "".into(),
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
        SlashCommandKind::Builtin { .. } => SkillDescriptor {
            name: cmd.name.clone(),
            description: cmd.description.clone(),
            disable_model_invocation: cmd.disable_model_invocation,
            command_type: SkillCommandType::Other,
            session_id,
            ..SkillDescriptor::default()
        },
        SlashCommandKind::Mcp { .. } => SkillDescriptor {
            name: cmd.name.clone(),
            description: cmd.description.clone(),
            disable_model_invocation: cmd.disable_model_invocation,
            command_type: SkillCommandType::Other,
            skip_shell_expansion: true,
            session_id,
            ..SkillDescriptor::default()
        },
    };
    if cmd.source == CommandSource::Plugin {
        if let Some(root) = descriptor.skill_root.as_deref() {
            descriptor.body = append_bundled_references(descriptor.body, root);
        }
    }
    descriptor
}

/// Mobile Plugin agents can intentionally omit filesystem tools, so a
/// file-backed skill that only says "read references/router.md" is not usable
/// when preloaded. Inline the verified Markdown reference closure into the
/// descriptor. Desktop remains file-backed; this is the mobile bundled form.
fn append_bundled_references(body: Utf16JsonProjection, skill_root: &Path) -> Utf16JsonProjection {
    let mut units = body
        .string_units("")
        .expect("skill prompt is a root string");
    let references = skill_root.join("references");
    let mut pending = vec![references.clone()];
    let mut files = Vec::new();
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_dir() {
                pending.push(path);
            } else if metadata.file_type().is_file()
                && path.extension().and_then(|ext| ext.to_str()) == Some("md")
            {
                files.push(path);
            }
        }
    }
    files.sort();
    for path in files {
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let relative = path.strip_prefix(skill_root).unwrap_or(&path).display();
        units.extend(format!("\n\n## Bundled resource: {relative}\n\n{contents}").encode_utf16());
    }
    projection_from_units(units)
}

fn product_prompt_token(suffix: &str) -> String {
    format!(
        "${{{}_{suffix}}}",
        branding::PRODUCT_NAME.to_ascii_uppercase()
    )
}

/// [`SkillLoader`] backed by the shared live mobile [`CommandRegistry`].
pub struct MobileDiskSkillLoader {
    registry: Arc<RwLock<CommandRegistry>>,
    command_snapshot_provider: Option<command_api::CommandSnapshotProvider>,
    session_id: Option<String>,
    prompt_cwd: Option<Arc<tool_api::SessionCwd>>,
    skill_usage_session: Option<command_api::skill_usage::SkillUsageSession>,
}

impl MobileDiskSkillLoader {
    #[must_use]
    pub fn new(registry: Arc<RwLock<CommandRegistry>>) -> Self {
        Self::for_mode(registry, SessionMode::Code)
    }

    #[must_use]
    pub fn for_mode(registry: Arc<RwLock<CommandRegistry>>, _session_mode: SessionMode) -> Self {
        Self {
            registry,
            command_snapshot_provider: None,
            session_id: None,
            prompt_cwd: None,
            skill_usage_session: None,
        }
    }

    #[must_use]
    pub fn with_session_id(registry: Arc<RwLock<CommandRegistry>>, session_id: String) -> Self {
        Self::with_session_id_and_mode(registry, session_id, SessionMode::Code)
    }

    #[must_use]
    pub fn with_session_id_and_mode(
        registry: Arc<RwLock<CommandRegistry>>,
        session_id: String,
        session_mode: SessionMode,
    ) -> Self {
        Self {
            session_id: Some(session_id),
            ..Self::for_mode(registry, session_mode)
        }
    }

    /// Supply the owning session's root and current directory for bundled prompts.
    pub fn with_prompt_cwd(mut self, cwd: Arc<tool_api::SessionCwd>) -> Self {
        self.prompt_cwd = Some(cwd);
        self
    }

    /// Refresh cwd-scoped command rows before this loader reads the registry.
    #[must_use]
    pub fn with_command_snapshot_provider(
        mut self,
        provider: command_api::CommandSnapshotProvider,
    ) -> Self {
        self.command_snapshot_provider = Some(provider);
        self
    }

    /// Share the session-scoped lifetime usage recorder with slash dispatch.
    #[must_use]
    pub fn with_skill_usage_session(
        mut self,
        session: command_api::skill_usage::SkillUsageSession,
    ) -> Self {
        self.skill_usage_session = Some(session);
        self
    }

    /// Construct a disk-populated live registry shared by conversation modes.
    pub async fn load_from_disk(
        cwd: &Path,
        lingxi_home: &Path,
        home: &Path,
        session_id: Option<String>,
        _session_mode: SessionMode,
    ) -> Self {
        let registry = Arc::new(RwLock::new(CommandRegistry::new()));
        {
            let mut reg = registry.write().await;
            load_mobile_disk_commands_into_registry(&mut reg, cwd, lingxi_home, home).await;
        }
        Self {
            registry,
            command_snapshot_provider: None,
            session_id,
            prompt_cwd: Some(tool_api::SessionCwd::new(cwd.to_path_buf(), Vec::new())),
            skill_usage_session: None,
        }
    }
}

/// Load the mobile on-disk prompt commands into `registry`.
///
/// Verified Plugin skills are registered separately by `PluginManager` after
/// this base load and therefore remain the authoritative file-backed source.
pub async fn load_mobile_disk_commands_into_registry(
    registry: &mut CommandRegistry,
    cwd: &Path,
    lingxi_home: &Path,
    home: &Path,
) {
    let no_managed = lingxi_home.join("__lingxi_no_managed_settings__");
    command_api::builtins::load_and_register_custom_commands(
        registry,
        cwd,
        lingxi_home,
        &no_managed,
        home,
    )
    .await;
    command_api::builtins::load_and_register_skill_commands_with_roots(
        registry,
        cwd,
        lingxi_home,
        None,
        home,
        &[],
    )
    .await;
}

#[async_trait::async_trait]
impl ToolSkillLoader for MobileDiskSkillLoader {
    async fn skill_invoked(
        &self,
        canonical_name: &Utf16JsonProjection,
        _input_name: &Utf16JsonProjection,
    ) {
        if let Some(session) = self.skill_usage_session.as_ref() {
            if let Err(error) = session.record(canonical_name) {
                tracing::warn!(%error, "failed to record mobile Skill-tool usage");
            }
        }
    }

    // Supply canonical names for the Skill tool's unknown-name suggestions.
    // Read the live registry and apply the same visibility rules as load().
    async fn list_names(&self) -> Vec<Utf16JsonProjection> {
        if let Some(provider) = &self.command_snapshot_provider {
            provider().await;
        }
        let registry = self.registry.read().await;
        registry
            .list_all()
            .into_iter()
            .filter(|command| registry.session_skill_allowed(command))
            .map(|command| command.name.clone())
            .collect()
    }

    async fn load(&self, name: &Utf16JsonProjection) -> Result<Option<SkillDescriptor>, ToolError> {
        if let Some(provider) = &self.command_snapshot_provider {
            provider().await;
        }
        let reg = self.registry.read().await;
        Ok(reg.resolve(name).and_then(|cmd| {
            reg.session_skill_allowed(cmd)
                .then(|| to_descriptor(cmd, self.session_id.as_deref()))
        }))
    }
}

impl MobileDiskSkillLoader {
    fn resolve_agent_skill_name(
        registry: &CommandRegistry,
        skill_name: &str,
        agent_type: &str,
    ) -> Option<Utf16JsonProjection> {
        let skill_name_source = Utf16JsonProjection::from(skill_name);
        if let Some(command) = registry.resolve(&skill_name_source) {
            return Some(command.name.clone());
        }
        if let Some(plugin_name) = agent_type.split(':').next().filter(|name| !name.is_empty()) {
            let qualified = Utf16JsonProjection::from(format!("{plugin_name}:{skill_name}"));
            if let Some(command) = registry.resolve(&qualified) {
                return Some(command.name.clone());
            }
        }
        let suffix = format!(":{skill_name}").encode_utf16().collect::<Vec<_>>();
        registry
            .list_all()
            .into_iter()
            .find(|command| {
                command
                    .name
                    .string_units("")
                    .is_some_and(|units| units.ends_with(&suffix))
            })
            .map(|command| command.name.clone())
    }
}

#[async_trait::async_trait]
impl AgentSkillLoader for MobileDiskSkillLoader {
    async fn resolve_and_load(
        &self,
        skill_name: &str,
        agent_type: &str,
        cwd: Option<&std::path::Path>,
        model: Option<&str>,
        child_available_tool_names: &[Utf16JsonProjection],
        cancellation: lingxi_core::host::CancellationToken,
    ) -> Result<Option<SkillLoad>, String> {
        if let Some(provider) = &self.command_snapshot_provider {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err("Agent skill preload cancelled".into()),
                _ = provider() => {}
            }
        }
        let registry = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err("Agent skill preload cancelled".into()),
            registry = self.registry.read() => registry,
        };
        let Some(resolved) = Self::resolve_agent_skill_name(&registry, skill_name, agent_type)
        else {
            return Ok(None);
        };
        let Some(command) = registry.resolve(&resolved).cloned() else {
            return Ok(None);
        };
        drop(registry);
        let descriptor = to_descriptor(&command, self.session_id.as_deref());
        if descriptor.command_type != SkillCommandType::Prompt {
            return Ok(None);
        }
        let mut body = match descriptor.dynamic_body {
            Some(builder) => {
                let cwd = cwd.map(std::path::Path::to_path_buf).unwrap_or_else(|| {
                    self.prompt_cwd.as_ref().map_or_else(
                        || std::env::current_dir().unwrap_or_default(),
                        |state| state.cwd(),
                    )
                });
                let root = self
                    .prompt_cwd
                    .as_ref()
                    .map_or_else(|| cwd.clone(), |state| state.project_root());
                builder
                    .try_build_at(
                        &Utf16JsonProjection::from(""),
                        command_api::BundledPromptContext {
                            project_root: &root,
                            cwd: &cwd,
                            is_preload: true,
                            main_loop_model: model,
                            available_tool_names: Some(child_available_tool_names),
                            workflow_authoring_base_allowed: false,
                        },
                    )
                    .map_err(|error| error.to_string())?
            }
            None => command_api::substitute_arguments_source(
                &descriptor.body,
                &Utf16JsonProjection::from(""),
                true,
                &descriptor.argument_names,
            )
            .map_err(|error| error.to_string())?,
        };
        if let Some(root) = descriptor.skill_root {
            let root = root.to_string_lossy();
            let root = if cfg!(windows) {
                root.replace('\\', "/")
            } else {
                root.into_owned()
            };
            body = replace_prompt_text(&body, &product_prompt_token("SKILL_DIR"), &root);
        }
        if let Some(session_id) = descriptor.session_id {
            body = replace_prompt_text(&body, &product_prompt_token("SESSION_ID"), &session_id);
        }
        if cancellation.is_cancelled() {
            return Err("Agent skill preload cancelled".into());
        }
        let body_units = body
            .string_units("")
            .expect("preloaded skill prompt is a root string");
        Ok(Some(SkillLoad {
            display_name: skill_name.into(),
            progress_message: None,
            content: vec![ContentBlock::TextJsUtf16 {
                text: String::from_utf16_lossy(&body_units),
                utf16_code_units: body_units,
                citations: None,
            }],
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use command_api::CommandFrontmatter;

    const FIXTURE_PLUGIN: &str = "acme-plugin";

    /// A namespaced Plugin skill registered the way `PluginManager` does it,
    /// with an in-memory body so the loader tests need no installed plugin.
    async fn loaded_fixture_plugin_loader(skills: &[(&str, &str)]) -> MobileDiskSkillLoader {
        let plugin_id = lingxi_core::types::PluginId::new();
        let commands = skills
            .iter()
            .map(|(name, body)| {
                let file = std::path::PathBuf::from(format!("plugin/skills/{name}/SKILL.md"));
                SlashCommand {
                    name: format!("{FIXTURE_PLUGIN}:{name}").into(),
                    source: CommandSource::Plugin,
                    kind: SlashCommandKind::Plugin {
                        plugin_id,
                        file_path: file,
                        frontmatter: CommandFrontmatter::default(),
                        prompt_template: (*body).to_owned().into(),
                    },
                    loaded_from: Some("plugin".to_owned()),
                    ..SlashCommand::default()
                }
            })
            .collect();
        let registry = Arc::new(RwLock::new(CommandRegistry::new()));
        registry
            .write()
            .await
            .register_plugin_commands(plugin_id, commands);
        MobileDiskSkillLoader::new(registry)
    }

    #[tokio::test]
    async fn mobile_skill_tool_suggests_canonical_plugin_name_and_accepts_retry() {
        use tool_api::test_support::{fresh_ctx, shell_test_ctx};
        use tool_api::tool_trait::Tool;

        let loader = Arc::new(loaded_fixture_plugin_loader(&[("canvas-skill", "Draw.")]).await);
        let canonical = "acme-plugin:canvas-skill";
        assert!(loader
            .list_names()
            .await
            .contains(&Utf16JsonProjection::from(canonical)));
        assert!(loader
            .load(&Utf16JsonProjection::from("canvas-skill"))
            .await
            .unwrap()
            .is_none());
        let tool = tool_skill::skill::SkillTool::with_loader(
            shell_test_ctx(mobile_linux_api::ProcessOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            }),
            loader,
        );
        let error = tool
            .validate_input(
                &Utf16JsonProjection::from_serde_value(
                    &serde_json::json!({ "skill": "canvas-skill" }),
                )
                .unwrap(),
                &fresh_ctx(),
            )
            .await
            .expect_err("bare plugin names must suggest an exact retry");
        assert!(error
            .0
            .string_at("")
            .expect("validation message is well-formed source text")
            .contains(canonical));
        tool.validate_input(
            &Utf16JsonProjection::from_serde_value(&serde_json::json!({ "skill": canonical }))
                .expect("generated test input is JSON"),
            &fresh_ctx(),
        )
        .await
        .expect("the suggested canonical name must validate");
    }

    #[tokio::test]
    async fn list_names_observes_registry_updates() {
        let registry = Arc::new(RwLock::new(CommandRegistry::new()));
        let loader = MobileDiskSkillLoader::new(registry.clone());
        assert!(loader.list_names().await.is_empty());
        registry.write().await.register_command(SlashCommand {
            name: "later-skill".into(),
            ..SlashCommand::default()
        });
        assert_eq!(loader.list_names().await, vec!["later-skill".into()]);
    }

    #[tokio::test]
    async fn skill_tool_usage_uses_canonical_name_not_the_input_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let session = command_api::skill_usage::SkillUsageSession::new(tmp.path().to_path_buf());
        let loader = MobileDiskSkillLoader::new(Arc::new(RwLock::new(CommandRegistry::new())))
            .with_skill_usage_session(session);
        loader
            .skill_invoked(
                &Utf16JsonProjection::from("acme-plugin:review"),
                &Utf16JsonProjection::from("review"),
            )
            .await;

        let usage = command_api::skill_usage::read_skill_usage(tmp.path()).unwrap();
        assert_eq!(
            usage
                .get(&"acme-plugin:review".encode_utf16().collect::<Vec<_>>())
                .map(|record| record.count),
            Some(1)
        );
        assert!(!usage.contains_key(&"review".encode_utf16().collect::<Vec<_>>()));
    }

    /// The deferred #14 test: a disk-authored `.lingxi/commands/*.md` under the
    /// app-private root resolves through the loader as a prompt skill (proving the
    /// mobile Skill tool is no longer inert), and an unknown name resolves to None.
    #[tokio::test]
    async fn resolves_on_disk_command_as_prompt_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cmd_dir = root.join(".lingxi").join("commands");
        tokio::fs::create_dir_all(&cmd_dir).await.unwrap();
        tokio::fs::write(
            cmd_dir.join("review-pr.md"),
            "---\ndescription: Review a PR\n---\nReview $ARGUMENTS\n",
        )
        .await
        .unwrap();

        let registry = Arc::new(RwLock::new(CommandRegistry::new()));
        {
            let mut reg = registry.write().await;
            load_mobile_disk_commands_into_registry(&mut reg, root, &root.join(".lingxi"), root)
                .await;
        }
        let loader = MobileDiskSkillLoader::new(registry);

        let desc = loader
            .load(&"review-pr".into())
            .await
            .expect("load ok")
            .expect("disk command resolves as a skill");
        assert_eq!(desc.name.string_at(""), Some("review-pr"));
        assert_eq!(desc.command_type, SkillCommandType::Prompt);
        assert!(
            desc.body
                .string_at("")
                .unwrap()
                .contains("Review $ARGUMENTS"),
            "body carries the markdown prompt template: {:?}",
            desc.body
        );
        assert!(loader
            .load(&"does-not-exist".into())
            .await
            .expect("load ok")
            .is_none());
    }

    #[tokio::test]
    async fn mobile_plugin_preload_inlines_the_verified_reference_closure() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_root = tmp.path().join("frontend-design");
        std::fs::create_dir_all(skill_root.join("references/profiles")).unwrap();
        std::fs::write(
            skill_root.join("references/router.md"),
            "ROUTER-ONLY-MARKER",
        )
        .unwrap();
        std::fs::write(
            skill_root.join("references/profiles/ios.md"),
            "IOS-PROFILE-ONLY-MARKER",
        )
        .unwrap();
        let plugin_id = lingxi_core::types::PluginId::new();
        let command = SlashCommand {
            name: "acme-plugin:frontend-design".into(),
            source: CommandSource::Plugin,
            skill_root: Some(skill_root.clone()),
            kind: SlashCommandKind::Plugin {
                plugin_id,
                file_path: skill_root.join("SKILL.md"),
                frontmatter: Default::default(),
                prompt_template: Utf16JsonProjection::parse(
                    r#""Read \ud800 references/router.md""#,
                )
                .unwrap(),
            },
            ..SlashCommand::default()
        };
        let registry = Arc::new(RwLock::new(CommandRegistry::new()));
        registry
            .write()
            .await
            .register_plugin_commands(plugin_id, vec![command]);
        let loader = MobileDiskSkillLoader::new(registry);

        let preload = AgentSkillLoader::resolve_and_load(
            &loader,
            "frontend-design",
            "acme-plugin:designer",
            None,
            None,
            &[],
            lingxi_core::host::CancellationToken::new(),
        )
        .await
        .expect("checked skill preload")
        .expect("namespaced Plugin skill must preload");
        assert!(matches!(
            preload.content.as_slice(),
            [ContentBlock::TextJsUtf16 { text, utf16_code_units, .. }]
                if utf16_code_units.contains(&0xd800)
                    && text.contains("ROUTER-ONLY-MARKER")
                    && text.contains("IOS-PROFILE-ONLY-MARKER")
                    && text.contains("## Bundled resource: references/router.md")
        ));
    }

    /// A namespaced Plugin skill must win over a user command with the same
    /// short name, through the mobile preload path.
    #[tokio::test]
    async fn namespaced_plugin_skill_loads_its_body_over_short_name_collision() {
        let loader = loaded_fixture_plugin_loader(&[("apple-design", "PLUGIN-OWNED-BODY")]).await;
        let registry = loader.registry.clone();
        registry.write().await.register_command(SlashCommand {
            name: "apple-design".into(),
            source: CommandSource::Builtin,
            kind: SlashCommandKind::Markdown {
                file_path: std::path::PathBuf::from("user/apple-design/SKILL.md"),
                frontmatter: CommandFrontmatter::default(),
                prompt_template: "USER SHORT-NAME SKILL — must never be selected".into(),
            },
            ..SlashCommand::default()
        });

        let loaded = AgentSkillLoader::resolve_and_load(
            &loader,
            "acme-plugin:apple-design",
            "acme-plugin:designer",
            None,
            None,
            &[],
            lingxi_core::host::CancellationToken::new(),
        )
        .await
        .expect("checked skill preload")
        .expect("installed namespaced skill must resolve");
        let body = loaded
            .content
            .iter()
            .find_map(|block| match block {
                ContentBlock::TextJsUtf16 { text, .. } => Some(text),
                _ => None,
            })
            .expect("plugin skill preload must be text");

        assert!(body.contains("PLUGIN-OWNED-BODY"));
        assert!(!body.contains("USER SHORT-NAME SKILL"));
    }

    #[tokio::test]
    async fn mobile_skill_catalog_and_descriptors_are_shared_across_modes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cmd_dir = root.join(".lingxi").join("commands");
        tokio::fs::create_dir_all(&cmd_dir).await.unwrap();
        tokio::fs::write(
            cmd_dir.join("review-pr.md"),
            "---\ndescription: Review a PR\nallowed-tools: Read, Write\n---\nReview $ARGUMENTS\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            cmd_dir.join("forked.md"),
            "---\ndescription: Forked\ncontext: fork\nbackground: true\n---\nInspect !`git status`\n",
        )
        .await
        .unwrap();
        let registry = Arc::new(RwLock::new(CommandRegistry::new()));
        {
            let mut commands = registry.write().await;
            load_mobile_disk_commands_into_registry(
                &mut *commands,
                root,
                &root.join(".lingxi"),
                root,
            )
            .await;
        }
        let chat = MobileDiskSkillLoader::for_mode(registry.clone(), SessionMode::Chat);
        let code = MobileDiskSkillLoader::for_mode(registry, SessionMode::Code);
        assert_eq!(chat.list_names().await, code.list_names().await);
        let chat_desc = chat.load(&"review-pr".into()).await.unwrap().unwrap();
        let code_desc = code.load(&"review-pr".into()).await.unwrap().unwrap();
        assert_eq!(chat_desc.allowed_tools, code_desc.allowed_tools);
        assert_eq!(
            chat_desc.skip_shell_expansion,
            code_desc.skip_shell_expansion
        );
        assert_eq!(chat_desc.context, code_desc.context);
        let chat_fork = chat.load(&"forked".into()).await.unwrap().unwrap();
        let code_fork = code.load(&"forked".into()).await.unwrap().unwrap();
        assert_eq!(chat_fork.context.as_deref(), Some("fork"));
        assert_eq!(chat_fork.context, code_fork.context);
        assert_eq!(chat_fork.background, Some(true));
        assert!(!chat_fork.skip_shell_expansion);
        assert_eq!(chat_fork.body, code_fork.body);
    }

    #[tokio::test]
    async fn mobile_bundled_preload_checks_paths_and_propagates_read_errors() {
        struct Checked;
        impl command_api::BundledPromptFn for Checked {
            fn build(&self, _: &Utf16JsonProjection) -> Utf16JsonProjection {
                panic!("unchecked preload");
            }
            fn try_build_at(
                &self,
                _: &Utf16JsonProjection,
                context: command_api::BundledPromptContext<'_>,
            ) -> std::io::Result<Utf16JsonProjection> {
                let root = context.project_root;
                let cwd = context.cwd;
                let preload = context.is_preload;
                assert_eq!(context.main_loop_model, Some("claude-fable-5"));
                assert!(preload);
                assert_eq!(root, Path::new("/project"));
                assert_eq!(cwd, Path::new("/child"));
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "loop.md denied",
                ))
            }
        }
        let mut reg = CommandRegistry::new();
        reg.register_command(SlashCommand {
            name: "loop".into(),
            source: command_api::CommandSource::Bundled,
            kind: SlashCommandKind::Bundled {
                frontmatter: Default::default(),
                prompt_fn: Some(Arc::new(Checked)),
            },
            ..Default::default()
        });
        let loader = MobileDiskSkillLoader::new(Arc::new(RwLock::new(reg)))
            .with_prompt_cwd(tool_api::SessionCwd::new("/project".into(), Vec::new()));
        assert_eq!(
            AgentSkillLoader::resolve_and_load(
                &loader,
                "loop",
                "agent",
                Some(Path::new("/child")),
                Some("claude-fable-5"),
                &[],
                lingxi_core::host::CancellationToken::new()
            )
            .await
            .unwrap_err(),
            "loop.md denied"
        );
    }
}
