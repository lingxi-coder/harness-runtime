//! Session-owned prompt directories, shared by `/add-dir` and its live catalogs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent::AgentSource;
use command_api::model::BuiltinCommandHandler;
use command_api::{CommandRegistry, CommandResult};
use tokio::sync::RwLock;

use super::{DesktopRepoRootReloader, DesktopRuntime};

/// Whether an additional directory changed the session's prompt catalogs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptDirectoryRegistration {
    /// Enabled prompt producers have been reloaded from this root.
    Registered,
    /// The exact directory was already registered in this session.
    AlreadyRegistered,
    /// Session customization policy disallows directory prompt discovery.
    Disabled,
}

/// Native `/add-dir` enables the skill/command and agent producers separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptDirectoryCapabilities {
    /// Enables additional skills and, outside bare mode, legacy commands.
    pub skills: bool,
    /// Enables additional custom-agent definitions.
    pub agents: bool,
}

impl PromptDirectoryCapabilities {
    fn enabled(self) -> bool {
        self.skills || self.agents
    }
}

impl DesktopRuntime {
    /// Check the discovery gate without probing an untrusted child symlink.
    #[must_use]
    pub fn prompt_directories_enabled(&self) -> bool {
        self.prompt_directory_capabilities().enabled()
    }

    /// Per-producer gates also select Native's exact `/add-dir` status text.
    #[must_use]
    pub fn prompt_directory_capabilities(&self) -> PromptDirectoryCapabilities {
        self.prompt_directory_reloader.prompt_directory_capabilities
    }

    /// Load a host-validated root into the existing live extension catalogs.
    /// This does not widen filesystem permission or add an MCP root.
    pub async fn register_prompt_directory(
        &self,
        root: PathBuf,
    ) -> Result<PromptDirectoryRegistration, String> {
        self.prompt_directory_reloader
            .register_prompt_directory(root)
            .await
    }

    /// Pure extension roots, kept separate from explicit permission grants.
    pub async fn registered_prompt_directories(&self) -> Vec<PathBuf> {
        self.prompt_directory_reloader
            .prompt_roots
            .read()
            .await
            .clone()
    }
}

impl DesktopRepoRootReloader {
    async fn register_prompt_directory(
        &self,
        root: PathBuf,
    ) -> Result<PromptDirectoryRegistration, String> {
        let capabilities = self.prompt_directory_capabilities;
        if !capabilities.enabled() {
            return Ok(PromptDirectoryRegistration::Disabled);
        }
        let _reload = self.catalog_reload_lock.lock().await;
        if self.native_skill_roots.contains(&root) || self.prompt_roots.read().await.contains(&root)
        {
            return Ok(PromptDirectoryRegistration::AlreadyRegistered);
        }
        validate_prompt_directories(&root, capabilities, self.bare).await?;
        let mut roots = self.native_skill_roots.clone();
        roots.extend(self.registered_roots.read().await.iter().cloned());
        roots.extend(self.prompt_roots.read().await.iter().cloned());
        if !roots.contains(&root) {
            roots.push(root.clone());
        }

        // Load before publishing either catalog. Project/user precedence is
        // the same as boot; flag, plugin and policy definitions remain owned
        // by their existing producers.
        let loaded_agents = if capabilities.agents {
            let agent_directories = agent::catalog::agent_dir_precedence(
                self.lingxi_home.join("agents"),
                &self.cwd,
                &self.home,
                permission::set_cwd::project_root_of(&self.cwd).as_deref(),
                &roots,
            );
            let disk_agents = agent::load_agents_from_dirs(&agent_directories).await;
            let managed_agents = match &self.managed_dir {
                Some(managed) => {
                    agent::load_agents_from_dirs(&[(
                        agent::catalog::policy_agent_dir(managed),
                        AgentSource::Settings(lingxi_core::types::SettingsScope::Managed),
                    )])
                    .await
                }
                None => Vec::new(),
            };
            Some((disk_agents, managed_agents))
        } else {
            None
        };
        let skill_directories = extension_directories(&roots, "skills");
        let command_directories = extension_directories(&roots, "commands");
        let mut registry = self.registry.write().await;
        let replacement = if capabilities.skills {
            let staged = Arc::new(RwLock::new(registry.clone()));
            let handler = self.prompt_reload_handler(
                staged.clone(),
                &skill_directories,
                &command_directories,
            );
            let parsed = command_api::parse_slash_command(&"/reload-skills".into())
                .ok_or_else(|| "internal /reload-skills command parse failed".to_owned())?;
            match handler.handle(&parsed).await {
                CommandResult::Done { .. } => {}
                CommandResult::Failed { source } => return Err(source.to_string()),
                _ => return Err("prompt directory catalog reload did not complete".to_owned()),
            }
            let mut replacement = staged.read().await.clone();
            // Stored handlers must point at the live registry, not staging.
            replacement.register_builtin_handler(Arc::new(self.live_prompt_reload_handler(
                self.registry.clone(),
                &skill_directories,
                &command_directories,
            )));
            replacement.register_builtin_handler(Arc::new(
                command_api::builtins::SkillsHandler::with_all_roots(
                    self.cwd.clone(),
                    self.lingxi_home.clone(),
                    self.managed_dir.clone(),
                    skill_directories,
                )
                .with_additional_skills_only(self.bare),
            ));
            Some(replacement)
        } else {
            None
        };
        let mut agents = self.agent_catalog.write().await;
        if let Some((disk_agents, managed_agents)) = loaded_agents {
            let mut replacement_agents: Vec<_> = agents
                .iter()
                .filter(|definition| {
                    matches!(
                        definition.source,
                        AgentSource::BuiltIn | AgentSource::Plugin
                    )
                })
                .cloned()
                .collect();
            agent::catalog::merge_agents_later_wins(&mut replacement_agents, disk_agents);
            agent::catalog::merge_agents_later_wins(
                &mut replacement_agents,
                agents
                    .iter()
                    .filter(|definition| definition.source == AgentSource::Flag)
                    .cloned()
                    .collect(),
            );
            agent::catalog::merge_agents_later_wins(&mut replacement_agents, managed_agents);
            *agents = replacement_agents;
        }
        if let Some(replacement) = replacement {
            *registry = replacement;
        }
        self.prompt_roots.write().await.push(root);
        drop(agents);
        drop(registry);
        if capabilities.skills {
            self.workflow_command_snapshot.refresh().await;
        }
        Ok(PromptDirectoryRegistration::Registered)
    }

    fn prompt_reload_handler(
        &self,
        registry: Arc<RwLock<CommandRegistry>>,
        skills: &[PathBuf],
        commands: &[PathBuf],
    ) -> command_api::builtins::ReloadSkillsHandler {
        command_api::builtins::ReloadSkillsHandler::with_all_roots(
            registry,
            self.cwd.clone(),
            self.lingxi_home.clone(),
            self.managed_dir.clone(),
            self.home.clone(),
            skills.to_vec(),
            self.safe_mode,
        )
        .with_additional_command_dirs(commands.to_vec())
        .with_additional_skills_only(self.bare)
    }

    pub(super) fn live_prompt_reload_handler(
        &self,
        registry: Arc<RwLock<CommandRegistry>>,
        skills: &[PathBuf],
        commands: &[PathBuf],
    ) -> command_api::builtins::ReloadSkillsHandler {
        let handler = self.prompt_reload_handler(registry, skills, commands);
        match self.reload_finalizer.get() {
            Some(finalizer) => handler.with_locked_post_reload_finalizer(true, finalizer.clone()),
            None => handler,
        }
    }
}

/// SessionStart must use the live handler, whose roots and reload finalizer
/// were assembled together with the catalog it is about to replace.
pub(super) async fn reload_skills_after_session_start(
    registry: &Arc<RwLock<CommandRegistry>>,
    session_start: &hooks::response::AggregateHookResult,
) -> Option<CommandResult> {
    if !session_start.reload_skills {
        return None;
    }
    let handler = registry.read().await.get_handler(&"reload-skills".into())?;
    let parsed = command_api::parse_slash_command(&"/reload-skills".into())?;
    Some(handler.handle(&parsed).await)
}

fn extension_directories(roots: &[PathBuf], directory: &str) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    for root in roots {
        let path = root.join(branding::DOT_DIR).join(directory);
        if !directories.contains(&path) {
            directories.push(path);
        }
    }
    directories
}

async fn validate_prompt_directories(
    root: &Path,
    capabilities: PromptDirectoryCapabilities,
    bare: bool,
) -> Result<(), String> {
    let metadata = tokio::fs::metadata(root)
        .await
        .map_err(|error| error.to_string())?;
    if !metadata.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    let roots = [root.to_path_buf()];
    let mut directories = Vec::new();
    if capabilities.skills {
        directories.extend(extension_directories(&roots, "skills"));
        if !bare {
            directories.extend(extension_directories(&roots, "commands"));
        }
    }
    if capabilities.agents {
        directories.extend(extension_directories(&roots, "agents"));
    }
    for directory in directories {
        match tokio::fs::read_dir(&directory).await {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot load {}: {error}", directory.display())),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(enabled: bool) -> (tempfile::TempDir, DesktopRepoRootReloader) {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().join("project");
        let home = directory.path().join("home");
        let config = home.join(branding::DOT_DIR);
        let managed = directory.path().join("managed");
        fs::create_dir_all(cwd.join(".git")).unwrap();
        fs::create_dir_all(&config).unwrap();
        fs::create_dir_all(&managed).unwrap();
        let registry = Arc::new(RwLock::new(CommandRegistry::new()));
        let current_cwd = Arc::new(std::sync::Mutex::new(cwd.clone()));
        let workflow_tool = Arc::new(
            tool_workflow::WorkflowTool::new(None)
                .with_current_cwd(current_cwd.clone())
                .with_allowed_builtin_workflow_names(Vec::new()),
        );
        let snapshot = Arc::new(crate::workflow_catalog::WorkflowCommandSnapshot::new(
            registry.clone(),
            Arc::new(RwLock::new(Some(workflow_tool.clone()))),
            current_cwd,
        ));
        let mut reloader = DesktopRepoRootReloader::new(
            registry,
            cwd,
            config,
            Vec::new(),
            false,
            false,
            snapshot.clone(),
            Arc::new(RwLock::new(Vec::new())),
            PromptDirectoryCapabilities {
                skills: enabled,
                agents: enabled,
            },
            Vec::new(),
        );
        reloader.home = home;
        reloader.managed_dir = Some(managed);
        reloader.set_reload_finalizer(snapshot.locked_post_reload_finalizer(workflow_tool));
        (directory, reloader)
    }

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(branding::DOT_DIR).join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn write_agent(root: &Path, name: &str, description: &str) {
        write(
            root,
            &format!("agents/{name}.md"),
            &format!("---\nname: {name}\ndescription: {description}\n---\nAgent instructions.\n"),
        );
    }

    #[tokio::test]
    async fn prompt_directory_registers_all_three_live_catalogs_and_reloads_keep_the_root() {
        let (directory, reloader) = fixture(true);
        let extra = directory.path().join("extra");
        write(
            &extra,
            "commands/extra-command.md",
            "---\ndescription: Legacy command\n---\nCOMMAND BODY",
        );
        write(
            &extra,
            "skills/extra-skill/SKILL.md",
            "---\nname: extra-skill\ndescription: Added skill\n---\nSKILL BODY",
        );
        write_agent(&extra, "extra-agent", "Added agent");
        let registry = reloader.registry.clone();
        let agents = reloader.agent_catalog.clone();
        assert_eq!(
            reloader
                .register_prompt_directory(extra.clone())
                .await
                .unwrap(),
            PromptDirectoryRegistration::Registered
        );
        {
            let registry = registry.read().await;
            assert_eq!(
                registry
                    .resolve(&"extra-command".into())
                    .unwrap()
                    .loaded_from
                    .as_deref(),
                Some("commands_DEPRECATED")
            );
            assert_eq!(
                registry
                    .resolve(&"extra-skill".into())
                    .unwrap()
                    .loaded_from
                    .as_deref(),
                Some("skills")
            );
        }
        assert_eq!(
            agents
                .read()
                .await
                .iter()
                .find(|agent| agent.agent_type == "extra-agent")
                .unwrap()
                .source,
            AgentSource::AdditionalDirectory
        );
        assert_eq!(
            reloader
                .register_prompt_directory(extra.clone())
                .await
                .unwrap(),
            PromptDirectoryRegistration::AlreadyRegistered
        );
        write(&extra, "commands/later-command.md", "Later command body");
        write(
            &extra,
            "skills/later-skill/SKILL.md",
            "---\nname: later-skill\ndescription: Later skill\n---\nLATER BODY",
        );
        write(
            &reloader.cwd,
            "workflows/later-workflow.js",
            "export const meta = {name: 'later-workflow', description: 'Later workflow'};\nawait agent('run');\n",
        );
        let reload = registry
            .read()
            .await
            .get_handler(&"reload-skills".into())
            .unwrap();
        let parsed = command_api::parse_slash_command(&"/reload-skills".into()).unwrap();
        assert!(matches!(
            reload.handle(&parsed).await,
            CommandResult::Done { .. }
        ));
        let registry = registry.read().await;
        assert!(registry.resolve(&"extra-command".into()).is_some());
        assert!(registry.resolve(&"later-command".into()).is_some());
        assert!(registry.resolve(&"later-skill".into()).is_some());
        assert!(registry.resolve(&"later-workflow".into()).is_some());
    }

    #[tokio::test]
    async fn prompt_directory_failure_is_retryable_and_does_not_publish_a_partial_catalog() {
        let (directory, reloader) = fixture(true);
        let extra = directory.path().join("extra");
        write(&extra, "commands/partial-command.md", "COMMAND BODY");
        write(
            &extra,
            "skills/partial-skill/SKILL.md",
            "---\nname: partial-skill\ndescription: Partial\n---\nBODY",
        );
        fs::write(
            extra.join(branding::DOT_DIR).join("agents"),
            "not a directory",
        )
        .unwrap();
        assert!(reloader
            .register_prompt_directory(extra.clone())
            .await
            .is_err());
        assert!(reloader.prompt_roots.read().await.is_empty());
        assert!(reloader.registry.read().await.list_all().is_empty());
        assert!(reloader.agent_catalog.read().await.is_empty());
        fs::remove_file(extra.join(branding::DOT_DIR).join("agents")).unwrap();
        write_agent(&extra, "retry-agent", "Retry agent");
        assert_eq!(
            reloader.register_prompt_directory(extra).await.unwrap(),
            PromptDirectoryRegistration::Registered
        );
        assert!(reloader
            .registry
            .read()
            .await
            .resolve(&"partial-command".into())
            .is_some());
        assert!(reloader
            .agent_catalog
            .read()
            .await
            .iter()
            .any(|agent| agent.agent_type == "retry-agent"));
    }

    #[tokio::test]
    async fn prompt_directory_disabled_gate_does_not_probe_or_register_the_root() {
        let (_directory, reloader) = fixture(false);
        assert_eq!(
            reloader
                .register_prompt_directory("/does/not/exist".into())
                .await
                .unwrap(),
            PromptDirectoryRegistration::Disabled
        );
        assert!(reloader.prompt_roots.read().await.is_empty());
        assert!(reloader.registry.read().await.list_all().is_empty());
    }

    #[tokio::test]
    async fn prompt_directory_loads_each_enabled_producer_and_ignores_disabled_directories() {
        for capabilities in [
            PromptDirectoryCapabilities {
                skills: true,
                agents: false,
            },
            PromptDirectoryCapabilities {
                skills: false,
                agents: true,
            },
        ] {
            let (directory, mut reloader) = fixture(true);
            reloader.prompt_directory_capabilities = capabilities;
            let extra = directory.path().join("extra");
            write(&extra, "commands/extra-command.md", "COMMAND BODY");
            write(
                &extra,
                "skills/extra-skill/SKILL.md",
                "---\nname: extra-skill\ndescription: Skill\n---\nSKILL BODY",
            );
            write_agent(&extra, "extra-agent", "Agent");
            for (enabled, name) in [
                (capabilities.skills, "skills"),
                (capabilities.skills, "commands"),
                (capabilities.agents, "agents"),
            ] {
                if !enabled {
                    let path = extra.join(branding::DOT_DIR).join(name);
                    fs::remove_dir_all(&path).unwrap();
                    fs::write(path, "disabled producer must not be read").unwrap();
                }
            }
            assert_eq!(
                reloader.register_prompt_directory(extra).await.unwrap(),
                PromptDirectoryRegistration::Registered
            );
            let registry = reloader.registry.read().await;
            assert_eq!(
                registry.resolve(&"extra-command".into()).is_some(),
                capabilities.skills
            );
            assert_eq!(
                registry.resolve(&"extra-skill".into()).is_some(),
                capabilities.skills
            );
            assert_eq!(
                reloader
                    .agent_catalog
                    .read()
                    .await
                    .iter()
                    .any(|definition| definition.agent_type == "extra-agent"),
                capabilities.agents
            );
        }
    }

    #[tokio::test]
    async fn prompt_directory_bare_loads_only_explicit_skills_in_live_catalog_and_viewer() {
        let (directory, mut reloader) = fixture(true);
        reloader.bare = true;
        reloader.prompt_directory_capabilities.agents = false;
        let extra = directory.path().join("extra");
        for (root, name) in [(&reloader.cwd, "project-skill"), (&extra, "extra-skill")] {
            write(
                root,
                &format!("skills/{name}/SKILL.md"),
                &format!("---\nname: {name}\ndescription: Skill\n---\nBODY"),
            );
            write(root, &format!("commands/{name}-command.md"), "LEGACY");
        }
        let user_skill = reloader.lingxi_home.join("skills/user-skill");
        fs::create_dir_all(&user_skill).unwrap();
        fs::write(
            user_skill.join("SKILL.md"),
            "---\nname: user-skill\ndescription: User skill\n---\nBODY",
        )
        .unwrap();
        write_agent(&extra, "extra-agent", "Must remain disabled");
        assert_eq!(
            reloader.register_prompt_directory(extra).await.unwrap(),
            PromptDirectoryRegistration::Registered
        );
        let (viewer, reload) = {
            let registry = reloader.registry.read().await;
            assert!(registry.resolve(&"extra-skill".into()).is_some());
            for name in [
                "project-skill",
                "user-skill",
                "project-skill-command",
                "extra-skill-command",
            ] {
                assert!(registry.resolve(&name.into()).is_none(), "{name}");
            }
            (
                registry.get_handler(&"skills".into()).unwrap(),
                registry.get_handler(&"reload-skills".into()).unwrap(),
            )
        };
        let parsed = command_api::parse_slash_command(&"/skills".into()).unwrap();
        let CommandResult::Done {
            display: Some(display),
        } = viewer.handle(&parsed).await
        else {
            panic!("skills viewer did not render its live roots");
        };
        let display = display.string_at("").unwrap();
        assert!(display.contains("extra-skill"));
        assert!(!display.contains("project-skill"));
        assert!(!display.contains("user-skill"));
        let parsed = command_api::parse_slash_command(&"/reload-skills".into()).unwrap();
        assert!(matches!(
            reload.handle(&parsed).await,
            CommandResult::Done { .. }
        ));
        let registry = reloader.registry.read().await;
        assert!(registry.resolve(&"extra-skill".into()).is_some());
        assert!(registry.resolve(&"project-skill".into()).is_none());
        assert!(registry.resolve(&"extra-skill-command".into()).is_none());
        assert!(reloader.agent_catalog.read().await.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn session_start_hook_reload_keeps_remount_only_roots_and_live_finalizer() {
        let (_directory, reloader) = fixture(true);
        let child = reloader.cwd.join("child");
        write(
            &child,
            "skills/remount-skill/SKILL.md",
            "---\nname: remount-skill\ndescription: Remounted skill\n---\nBODY",
        );
        write(&child, "commands/remount-command.md", "REMOUNT COMMAND");
        // Model a host remount's private initial_prompt_directories. The root
        // is registered without appearing in permission-grant/add_dir inputs.
        assert!(reloader.native_skill_roots.is_empty());
        reloader.prompt_roots.write().await.push(child.clone());
        let skills = extension_directories(&[child.clone()], "skills");
        let commands = extension_directories(&[child.clone()], "commands");
        let reload = Arc::new(reloader.live_prompt_reload_handler(
            reloader.registry.clone(),
            &skills,
            &commands,
        ));
        reloader
            .registry
            .write()
            .await
            .register_builtin_handler(reload.clone());
        let parsed = command_api::parse_slash_command(&"/reload-skills".into()).unwrap();
        assert!(matches!(
            reload.handle(&parsed).await,
            CommandResult::Done { .. }
        ));
        assert!(reloader
            .registry
            .read()
            .await
            .resolve(&"remount-skill".into())
            .is_some());

        write(
            &reloader.cwd,
            "workflows/from-hook-reload.js",
            "export const meta = {name: 'from-hook-reload', description: 'Hook reload'};\nawait agent('run');\n",
        );
        let source = serde_json::to_string(&serde_json::json!({
            "hooks":{"SessionStart":[{"hooks":[{
                "type":"command",
                "command":format!(
                    "mkdir -p child/{dot}/skills/from-hook && printf '%s\\n' '---' 'name: from-hook' 'description: Hook skill' '---' 'BODY' > child/{dot}/skills/from-hook/SKILL.md && printf '%s\\n' '{{\"hookSpecificOutput\":{{\"hookEventName\":\"SessionStart\",\"reloadSkills\":true}}}}'",
                    dot = branding::DOT_DIR,
                )
            }]}]}
        }))
        .unwrap();
        let mut hooks = hooks::HookRegistry::new();
        for hook in hooks::parse_hooks_from_settings_json(
            &source,
            hooks::HookSource::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap()
        {
            hooks.register(hook);
        }
        let hooks = hooks::HookExecutorImpl::new(
            Arc::new(RwLock::new(hooks)),
            Arc::new(platform_posix::PosixHttp::new()),
            Arc::new(platform_posix::PosixRuntime::new()),
        )
        .with_process_runner(
            Arc::new(super::super::platform::DesktopProcess::new()),
            Arc::new(platform_posix::PosixSandbox::new()),
        );
        let session_id = lingxi_core::types::SessionId::new();
        let result = hooks
            .execute(
                hooks::HookEvent::SessionStart {
                    session_id,
                    source: "startup".into(),
                },
                hooks::HookContext {
                    session_id,
                    cwd: reloader.cwd.clone(),
                    ..Default::default()
                },
            )
            .await;
        assert!(result.reload_skills, "{result:?}");
        assert!(matches!(
            reload_skills_after_session_start(&reloader.registry, &result).await,
            Some(CommandResult::Done { .. })
        ));
        let registry = reloader.registry.read().await;
        for name in [
            "remount-skill",
            "remount-command",
            "from-hook",
            "from-hook-reload",
        ] {
            assert!(registry.resolve(&name.into()).is_some(), "{name}");
        }
        assert_eq!(*reloader.prompt_roots.read().await, vec![child]);
    }

    #[tokio::test]
    async fn prompt_directory_keeps_project_command_and_agent_precedence_and_live_producers() {
        let (directory, reloader) = fixture(true);
        let extra = directory.path().join("extra");
        write(&reloader.cwd, "commands/same.md", "PROJECT COMMAND");
        write(&extra, "commands/same.md", "EXTRA COMMAND");
        write_agent(&reloader.cwd, "same-agent", "PROJECT");
        write_agent(&extra, "same-agent", "EXTRA");
        write_agent(&extra, "flag-agent", "EXTRA");
        let flag = agent::parse_agent_from_json(
            "flag-agent",
            &serde_json::json!({
                "description":"FLAG", "prompt":"Flag body"
            }),
            AgentSource::Flag,
        )
        .unwrap();
        let mut plugin = flag.clone();
        plugin.agent_type = "plugin:kept".into();
        plugin.source = AgentSource::Plugin;
        reloader.agent_catalog.write().await.extend([flag, plugin]);
        assert_eq!(
            reloader.register_prompt_directory(extra).await.unwrap(),
            PromptDirectoryRegistration::Registered
        );
        let commands = reloader.registry.read().await;
        let command = commands.resolve(&"same".into()).unwrap();
        assert!(
            matches!(&command.kind, command_api::model::SlashCommandKind::Markdown { file_path, .. } if file_path.starts_with(&reloader.cwd))
        );
        let agents = reloader.agent_catalog.read().await;
        assert_eq!(
            agents
                .iter()
                .find(|agent| agent.agent_type == "same-agent")
                .unwrap()
                .when_to_use,
            "PROJECT"
        );
        assert_eq!(
            agents
                .iter()
                .find(|agent| agent.agent_type == "flag-agent")
                .unwrap()
                .source,
            AgentSource::Flag
        );
        assert!(agents
            .iter()
            .any(|agent| agent.agent_type == "plugin:kept" && agent.source == AgentSource::Plugin));
    }
}
