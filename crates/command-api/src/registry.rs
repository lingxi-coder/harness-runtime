//! In-memory registry of slash commands with alias and plugin-scoped lookup.

use crate::model::{BuiltinCommandHandler, CommandSource, SlashCommand, SlashCommandKind};
use lingxi_core::types::PluginId;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Owns all known slash commands and built-in handler implementations.
pub struct CommandRegistry {
    commands: HashMap<String, SlashCommand>,
    aliases: HashMap<String, String>,
    builtin_handlers: HashMap<String, Arc<dyn BuiltinCommandHandler>>,
    plugin_commands: HashMap<PluginId, Vec<String>>,
    session_skill_allowlist: Option<Vec<String>>,
    /// Loaded plugin display name -> (owner id, storage identity, native provider).
    plugin_describe_providers: HashMap<String, (PluginId, String, Value)>,
    managed_mcp_servers: HashSet<String>,
    mod_commands: HashMap<String, (String, bool)>,
}

impl CommandRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            commands: HashMap::new(),
            aliases: HashMap::new(),
            builtin_handlers: HashMap::new(),
            plugin_commands: HashMap::new(),
            session_skill_allowlist: None,
            plugin_describe_providers: HashMap::new(),
            managed_mcp_servers: HashSet::new(),
            mod_commands: HashMap::new(),
        }
    }

    /// Insert a fully-formed [`SlashCommand`] (used for markdown / plugin / MCP
    /// entries), indexing every entry in `cmd.aliases` so the command resolves by
    /// any of its alternate names. Mirrors the TS `findCommand` search over
    /// `name` + `aliases` (`claude-code/src/commands.ts:690`).
    /// Answers: may a later load replace an already-registered command.
    ///
    /// One of several orderings over these rungs; `lingxi_core::types::scope`'s module docs index them all and say which question each answers.
    pub fn register_command(&mut self, cmd: SlashCommand) {
        if self.mod_commands.contains_key(&cmd.name) {
            return;
        }
        // Plugin names are qualified namespaces. A later project/user reload
        // must not replace the live owner behind `plugin:skill`; otherwise a
        // disk skill can impersonate a verified Plugin until the next boot.
        if self.commands.get(&cmd.name).is_some_and(|current| {
            current.source == CommandSource::Plugin && cmd.source != CommandSource::Plugin
        }) {
            return;
        }
        self.index_aliases(&cmd.name, &cmd.aliases);
        self.commands.insert(cmd.name.clone(), cmd);
    }

    /// Point each alias at `target` in the alias map (used by
    /// [`Self::register_command`] / [`Self::register_plugin_commands`]).
    fn index_aliases(&mut self, target: &str, aliases: &[String]) {
        for alias in aliases {
            if self.mod_commands.contains_key(alias) {
                continue;
            }
            self.aliases.insert(alias.clone(), target.to_string());
        }
    }

    /// Register a Rust-side built-in handler under its `name()`.
    pub fn register_builtin_handler(&mut self, h: Arc<dyn BuiltinCommandHandler>) {
        if self.mod_commands.contains_key(h.name()) {
            return;
        }
        let cmd = SlashCommand {
            name: h.name().to_string(),
            description: h.description().to_string(),
            argument_hint: h.argument_hint().map(str::to_string),
            source: CommandSource::Builtin,
            kind: SlashCommandKind::Builtin {
                handler_id: h.name().to_string(),
            },
            ..SlashCommand::default()
        };
        self.commands.insert(h.name().to_string(), cmd);
        self.builtin_handlers.insert(h.name().to_string(), h);
    }

    /// Make `alias` resolve to `target`.
    pub fn register_alias(&mut self, alias: String, target: String) {
        if self.mod_commands.contains_key(&alias) {
            return;
        }
        self.aliases.insert(alias, target);
    }

    /// Resolve a name (possibly an alias) to a registered command.
    #[must_use]
    pub fn resolve(&self, name: &str) -> Option<&SlashCommand> {
        let canon = self.aliases.get(name).map_or(name, String::as_str);
        self.commands.get(canon)
    }

    /// Fetch a built-in handler by id.
    ///
    /// Canonicalizes through the aliases map exactly like [`Self::resolve`] does,
    /// then looks up the built-in handler under the canonical name. For a
    /// non-alias `handler_id`, `canon == handler_id`, so behavior is identical to
    /// a plain `builtin_handlers` lookup; aliases (e.g. `continue` → `resume`)
    /// route to the target's handler.
    #[must_use]
    pub fn get_handler(&self, handler_id: &str) -> Option<Arc<dyn BuiltinCommandHandler>> {
        let canon = self
            .aliases
            .get(handler_id)
            .map_or(handler_id, String::as_str);
        self.builtin_handlers.get(canon).cloned()
    }

    /// Retire the executable builtin handler graph while preserving the command
    /// catalog, aliases, and plugin metadata.
    ///
    /// Hosts call this only after request/task/session-switch admission has been
    /// fenced. The returned handlers must be dropped after releasing any outer
    /// lock protecting this registry: handler destruction can tear down an
    /// orchestrator which itself owns a command-listing view of the registry.
    pub fn take_builtin_handlers(&mut self) -> Vec<Arc<dyn BuiltinCommandHandler>> {
        std::mem::take(&mut self.builtin_handlers)
            .into_values()
            .collect()
    }

    /// Register a batch of commands owned by `plugin_id`.
    pub fn register_plugin_commands(&mut self, plugin_id: PluginId, cmds: Vec<SlashCommand>) {
        let names: Vec<String> = cmds
            .iter()
            .filter(|command| !self.mod_commands.contains_key(&command.name))
            .map(|command| command.name.clone())
            .collect();
        for c in cmds {
            if self.mod_commands.contains_key(&c.name) {
                continue;
            }
            self.index_aliases(&c.name, &c.aliases);
            self.commands.insert(c.name.clone(), c);
        }
        self.plugin_commands.insert(plugin_id, names);
    }

    /// Owner of a command registered by a Mod, if any.
    #[must_use]
    pub fn mod_owner_of(&self, name: &str) -> Option<&str> {
        self.mod_commands.get(name).map(|(owner, _)| owner.as_str())
    }

    /// Whether a live Mod command executes immediately when selected.
    #[must_use]
    pub fn mod_immediate_of(&self, name: &str) -> Option<bool> {
        self.mod_commands.get(name).map(|(_, immediate)| *immediate)
    }

    /// Stamp the installed identity and authority seat resolved by the plugin
    /// loader. Declarative commands and Mod-registered commands share it.
    pub fn set_plugin_describe_provider(
        &mut self,
        plugin_id: PluginId,
        display_name: &str,
        storage_identity: &str,
        tier: &str,
    ) {
        self.plugin_describe_providers.insert(
            display_name.to_owned(),
            (
                plugin_id,
                storage_identity.to_owned(),
                json!({"plugin":storage_identity,"tier":tier}),
            ),
        );
    }

    fn plugin_describe_provider(&self, name: &str) -> Option<Value> {
        self.plugin_describe_providers
            .get(name)
            .map(|(_, _, provider)| provider.clone())
    }

    /// Preserve the native `enterprise`/`managed` MCP authority distinction
    /// for prompt commands contributed by this connected server.
    pub fn set_mcp_server_describe_scope(&mut self, server: &str, managed: bool) {
        if managed {
            self.managed_mcp_servers.insert(server.to_owned());
        } else {
            self.managed_mcp_servers.remove(server);
        }
    }

    /// Native `command.describe` origin for a catalog entry. A registered Mod
    /// takes precedence over the command's synthetic Builtin dispatch kind.
    #[must_use]
    pub fn mod_describe_provider(&self, command: &SlashCommand) -> Value {
        if let Some(owner) = self.mod_owner_of(&command.name) {
            return self
                .plugin_describe_provider(owner)
                .unwrap_or_else(|| json!({"plugin": owner, "tier": "user"}));
        }
        match command.source {
            CommandSource::Builtin | CommandSource::Bundled => {
                json!({"plugin": "engine", "tier": "core"})
            }
            CommandSource::Settings(lingxi_core::types::SettingsScope::Managed) => {
                json!({"plugin": "policy", "tier": "prepend"})
            }
            CommandSource::Settings(scope) => {
                let plugin = match scope {
                    lingxi_core::types::SettingsScope::User => "user",
                    lingxi_core::types::SettingsScope::Project => "project",
                    lingxi_core::types::SettingsScope::Local => "local",
                    lingxi_core::types::SettingsScope::Managed => unreachable!(),
                };
                json!({"plugin": plugin, "tier": "user"})
            }
            CommandSource::Plugin => {
                let owner = command
                    .name
                    .split_once(':')
                    .map_or("", |(plugin, _)| plugin);
                self.plugin_describe_provider(owner)
                    .unwrap_or_else(|| json!({"plugin":owner,"tier":"user"}))
            }
            CommandSource::Mcp => {
                let server = command
                    .name
                    .rsplit_once(':')
                    .map_or("", |(server, _)| server);
                if self.managed_mcp_servers.contains(server) {
                    return json!({"plugin":format!("mcp:{server}"),"tier":"prepend"});
                }
                if let Some(owner) = server
                    .strip_prefix("plugin:")
                    .and_then(|tail| tail.split_once(':').map(|(owner, _)| owner))
                {
                    if let Some(provider) = self.plugin_describe_provider(owner) {
                        return provider;
                    }
                }
                json!({"plugin":format!("mcp:{server}"),"tier":"user"})
            }
        }
    }

    /// Insert or replace a live Mod command after the caller has checked for
    /// non-Mod name and alias collisions.
    pub fn register_mod_command(
        &mut self,
        owner: &str,
        command: SlashCommand,
        handler: Arc<dyn BuiltinCommandHandler>,
        immediate: bool,
    ) {
        // A stale alias must not keep a newly registered Mod command pointing
        // at an unrelated command (or at a name that no longer exists).
        self.aliases.remove(&command.name);
        self.mod_commands
            .insert(command.name.clone(), (owner.to_owned(), immediate));
        self.builtin_handlers.insert(command.name.clone(), handler);
        self.commands.insert(command.name.clone(), command);
    }

    /// Remove the commands served by one unloaded Mod.
    pub fn unregister_mod_owner(&mut self, owner: &str) {
        let display_owner = self
            .plugin_describe_providers
            .iter()
            .find(|(_, (_, identity, _))| identity == owner)
            .map_or(owner, |(name, _)| name.as_str());
        let names = self
            .mod_commands
            .iter()
            .filter(|(_, (registered_owner, _))| registered_owner == display_owner)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for name in names {
            self.mod_commands.remove(&name);
            self.commands.remove(&name);
            self.builtin_handlers.remove(&name);
            self.aliases.retain(|_, target| target != &name);
        }
    }

    /// Every registered command, in arbitrary order. Mirrors TS `getCommands`
    /// returning the full command list before any UI/model filtering.
    #[must_use]
    pub fn list_all(&self) -> Vec<&SlashCommand> {
        self.commands.values().collect()
    }

    /// Commands the model is allowed to invoke — every entry whose
    /// `disable_model_invocation` flag is unset. Mirrors the TS filter
    /// `!cmd.disableModelInvocation` used when building the model-facing skill
    /// surface (`claude-code/src/commands.ts:571`).
    #[must_use]
    pub fn model_invocable_commands(&self) -> Vec<&SlashCommand> {
        self.commands
            .values()
            .filter(|c| !c.disable_model_invocation)
            .filter(|c| self.session_skill_allowed(c))
            .collect()
    }

    /// Host-owned session skill grants. `None` keeps every eligible skill;
    /// `Some([])` exposes none. Manual slash invocation remains available.
    pub fn set_session_skill_allowlist(&mut self, names: Option<Vec<String>>) {
        self.session_skill_allowlist = names;
    }

    /// Claude Code 2.1.286 `RX`: exact names, synced-skill aliases, or qualified
    /// suffixes for ordinary skills. MCP prompts require their exact name.
    #[must_use]
    pub fn session_skill_allowed(&self, command: &SlashCommand) -> bool {
        let Some(names) = self.session_skill_allowlist.as_ref() else {
            return true;
        };
        names.iter().any(|name| {
            command.name == *name
                || if command.loaded_from.as_deref() == Some("syncedSkills") {
                    command.aliases.contains(name)
                } else {
                    let prompt = matches!(
                        command.kind,
                        SlashCommandKind::Markdown { .. }
                            | SlashCommandKind::Plugin { .. }
                            | SlashCommandKind::Bundled { .. }
                            | SlashCommandKind::Mcp { .. }
                    );
                    !(prompt && command.source == CommandSource::Mcp)
                        && command.name.ends_with(&format!(":{name}"))
                }
        })
    }

    /// Commands exposed through the user-facing slash palette. This is a
    /// distinct projection from [`Self::model_invocable_commands`]: it keeps
    /// manual-only commands, excludes env-disabled builtins and
    /// `user_invocable = false`, and preserves hidden commands so callers can
    /// still match them on exact input.
    #[must_use]
    pub fn palette_commands(&self) -> Vec<SlashCommand> {
        self.commands
            .values()
            .filter(|command| command.user_invocable != Some(false))
            .filter(|command| {
                !matches!(command.kind, SlashCommandKind::Builtin { .. })
                    || !crate::builtin_support::names::is_command_env_disabled(&command.name)
            })
            .map(|command| {
                let mut command = command.clone();
                command.aliases = self.aliases_for(&command.name);
                command
            })
            .collect()
    }

    /// Every alias that resolves to `target`, including aliases declared on the
    /// command and aliases registered separately via [`Self::register_alias`].
    #[must_use]
    pub fn aliases_for(&self, target: &str) -> Vec<String> {
        let mut aliases = self
            .resolve(target)
            .map_or_else(Vec::new, |command| command.aliases.clone());
        for alias in crate::builtin_support::names::command_aliases(target) {
            if !aliases.iter().any(|existing| existing == alias) {
                aliases.push((*alias).to_string());
            }
        }
        for (alias, canonical) in &self.aliases {
            if canonical == target && !aliases.iter().any(|existing| existing == alias) {
                aliases.push(alias.clone());
            }
        }
        aliases.sort();
        aliases
    }

    /// Remove every command previously registered under `plugin_id`, along with
    /// any aliases that pointed at those commands.
    pub fn unregister_plugin(&mut self, plugin_id: &PluginId) {
        if let Some(names) = self.plugin_commands.remove(plugin_id) {
            for n in &names {
                self.commands.remove(n);
            }
            self.aliases.retain(|_, target| !names.contains(target));
        }
        self.plugin_describe_providers
            .retain(|_, (owner_id, _, _)| owner_id != plugin_id);
    }

    /// Remove only the prompt commands advertised by one disconnected MCP
    /// server, leaving same-prefix local and plugin commands intact.
    pub fn unregister_mcp_server_prompts(&mut self, server: &str) {
        let prefix = format!("{server}:");
        let names = self
            .commands
            .iter()
            .filter(|(name, command)| {
                name.starts_with(&prefix) && command.source == CommandSource::Mcp
            })
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for name in &names {
            self.commands.remove(name);
        }
        self.aliases.retain(|_, target| !names.contains(target));
        self.managed_mcp_servers.remove(server);
    }

    /// Replace the MCP prompt partition from a live connection snapshot.
    /// Local and plugin slash commands keep precedence over a same-named
    /// remote prompt, including when the server reconnects or changes its list.
    pub fn reconcile_mcp_prompt_commands(&mut self, prompts: Vec<(SlashCommand, bool)>) {
        let old_names = self
            .commands
            .iter()
            .filter(|(_, command)| command.source == CommandSource::Mcp)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for name in &old_names {
            self.commands.remove(name);
        }
        self.aliases.retain(|_, target| !old_names.contains(target));
        self.managed_mcp_servers.clear();
        for (command, managed) in prompts {
            if let Some((server, _)) = command.name.rsplit_once(':') {
                self.set_mcp_server_describe_scope(server, managed);
            }
            if self.resolve(&command.name).is_none() {
                self.register_command(command);
            }
        }
    }

    /// Remove every command attributed to `loaded_from`, including aliases
    /// pointing at those commands.
    ///
    /// Catalog refreshers use this before a full disk re-scan so deleted skill
    /// files disappear from the live registry instead of surviving forever as
    /// stale entries.
    pub fn unregister_loaded_from(&mut self, loaded_from: &str) -> usize {
        let names: Vec<String> = self
            .commands
            .iter()
            .filter(|(name, command)| {
                !self.mod_commands.contains_key(*name)
                    && command.loaded_from.as_deref() == Some(loaded_from)
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in &names {
            self.commands.remove(name);
        }
        self.aliases.retain(|_, target| !names.contains(target));
        names.len()
    }

    /// Remove non-Plugin commands attempting to occupy a reserved Plugin
    /// namespace. Hosts use this after disk reload so a disabled builtin
    /// plugin cannot be impersonated by a same-name project skill.
    pub fn unregister_non_plugin_prefix(&mut self, prefix: &str) -> usize {
        let names: Vec<String> = self
            .commands
            .iter()
            .filter(|(name, command)| {
                name.starts_with(prefix) && command.source != CommandSource::Plugin
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in &names {
            self.commands.remove(name);
        }
        self.aliases.retain(|_, target| !names.contains(target));
        names.len()
    }
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtin_support::unimplemented::UnimplementedCommandHandler;
    use crate::model::{CommandSource, SlashCommandKind};

    fn markdown_cmd(name: &str, aliases: Vec<String>) -> SlashCommand {
        SlashCommand {
            name: name.to_string(),
            description: format!("{name} cmd"),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            kind: SlashCommandKind::Builtin {
                handler_id: name.to_string(),
            },
            aliases,
            ..SlashCommand::default()
        }
    }

    #[test]
    fn command_describe_provider_uses_native_source_tiers_and_mod_owner() {
        let mut registry = CommandRegistry::new();
        let project = markdown_cmd("deploy", Vec::new());
        assert_eq!(
            registry.mod_describe_provider(&project),
            json!({"plugin":"project","tier":"user"})
        );
        let managed = SlashCommand {
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Managed),
            ..project.clone()
        };
        assert_eq!(
            registry.mod_describe_provider(&managed),
            json!({"plugin":"policy","tier":"prepend"})
        );
        let mcp = SlashCommand {
            name: "build:run".into(),
            source: CommandSource::Mcp,
            ..project.clone()
        };
        assert_eq!(
            registry.mod_describe_provider(&mcp),
            json!({"plugin":"mcp:build","tier":"user"})
        );
        let mod_command = SlashCommand {
            name: "custom".into(),
            source: CommandSource::Plugin,
            ..project
        };
        registry.register_mod_command(
            "demo",
            mod_command.clone(),
            Arc::new(UnimplementedCommandHandler::new("custom", "custom command")),
            true,
        );
        assert_eq!(registry.mod_immediate_of("custom"), Some(true));
        assert_eq!(
            registry.mod_describe_provider(&mod_command),
            json!({"plugin":"demo","tier":"user"})
        );
    }

    #[test]
    fn plugin_describe_provider_follows_installed_identity_and_unload() {
        let mut registry = CommandRegistry::new();
        let plugin_id = PluginId::new();
        registry.set_plugin_describe_provider(plugin_id, "demo", "demo@market", "append");
        let skill = SlashCommand {
            name: "demo:review".into(),
            source: CommandSource::Plugin,
            ..SlashCommand::default()
        };
        let mcp_prompt = SlashCommand {
            name: "plugin:demo:server:prompt".into(),
            source: CommandSource::Mcp,
            ..SlashCommand::default()
        };
        for command in [&skill, &mcp_prompt] {
            assert_eq!(
                registry.mod_describe_provider(command),
                json!({"plugin":"demo@market","tier":"append"})
            );
        }
        registry.set_mcp_server_describe_scope("plugin:demo:server", true);
        assert_eq!(
            registry.mod_describe_provider(&mcp_prompt),
            json!({"plugin":"mcp:plugin:demo:server","tier":"prepend"})
        );
        registry.set_mcp_server_describe_scope("plugin:demo:server", false);
        let mod_command = SlashCommand {
            name: "custom".into(),
            source: CommandSource::Plugin,
            ..SlashCommand::default()
        };
        registry.register_mod_command(
            "demo",
            mod_command.clone(),
            Arc::new(UnimplementedCommandHandler::new("custom", "custom command")),
            false,
        );
        assert_eq!(
            registry.mod_describe_provider(&mod_command),
            json!({"plugin":"demo@market","tier":"append"})
        );
        registry.unregister_mod_owner("demo@market");
        assert!(registry.resolve("custom").is_none());
        registry.unregister_plugin(&plugin_id);
        assert_eq!(
            registry.mod_describe_provider(&skill),
            json!({"plugin":"demo","tier":"user"})
        );
    }

    #[test]
    fn disconnect_removes_only_the_servers_prompt_commands() {
        let mut registry = CommandRegistry::new();
        let prompt = SlashCommand {
            name: "team:server:review".into(),
            source: CommandSource::Mcp,
            ..SlashCommand::default()
        };
        let local = SlashCommand {
            name: "team:server:local".into(),
            ..markdown_cmd("local", Vec::new())
        };
        registry.register_command(prompt.clone());
        registry.register_command(local);
        registry.set_mcp_server_describe_scope("team:server", true);
        assert_eq!(
            registry.mod_describe_provider(&prompt),
            json!({"plugin":"mcp:team:server","tier":"prepend"})
        );
        registry.unregister_mcp_server_prompts("team:server");
        assert!(registry.resolve(&prompt.name).is_none());
        assert!(registry.resolve("team:server:local").is_some());
    }

    #[test]
    fn reconnect_replaces_prompt_partition_and_keeps_local_shadow() {
        let mut registry = CommandRegistry::new();
        let old = SlashCommand {
            name: "srv:old".into(),
            source: CommandSource::Mcp,
            ..SlashCommand::default()
        };
        let shadow = SlashCommand {
            name: "srv:shadow".into(),
            ..markdown_cmd("shadow", Vec::new())
        };
        registry.register_command(shadow);
        registry.reconcile_mcp_prompt_commands(vec![(old.clone(), false)]);
        assert!(registry.resolve("srv:old").is_some());
        let replacement = SlashCommand {
            name: "srv:new".into(),
            source: CommandSource::Mcp,
            ..SlashCommand::default()
        };
        let remote_shadow = SlashCommand {
            name: "srv:shadow".into(),
            source: CommandSource::Mcp,
            ..SlashCommand::default()
        };
        registry.reconcile_mcp_prompt_commands(vec![
            (replacement.clone(), true),
            (remote_shadow, true),
        ]);
        assert!(registry.resolve("srv:old").is_none());
        assert!(registry.resolve("srv:new").is_some());
        assert_eq!(
            registry.resolve("srv:shadow").unwrap().source,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project)
        );
        assert_eq!(
            registry.mod_describe_provider(&replacement),
            json!({"plugin":"mcp:srv","tier":"prepend"})
        );
        registry.reconcile_mcp_prompt_commands(Vec::new());
        assert!(registry.resolve("srv:new").is_none());
        assert!(registry.resolve("srv:shadow").is_some());
    }

    /// A command registered with aliases resolves by its canonical name and by
    /// each alias (TS `findCommand` over `name` + `aliases`).
    #[test]
    fn register_command_indexes_each_alias() {
        let mut reg = CommandRegistry::new();
        reg.register_command(markdown_cmd(
            "resume",
            vec!["continue".to_string(), "unpause".to_string()],
        ));

        assert_eq!(
            reg.resolve("resume").map(|c| c.name.as_str()),
            Some("resume")
        );
        assert_eq!(
            reg.resolve("continue").map(|c| c.name.as_str()),
            Some("resume"),
            "alias `continue` should resolve to `resume`"
        );
        assert_eq!(
            reg.resolve("unpause").map(|c| c.name.as_str()),
            Some("resume"),
            "alias `unpause` should resolve to `resume`"
        );
        assert!(reg.resolve("missing").is_none());
    }

    /// `model_invocable_commands` excludes entries flagged
    /// `disable_model_invocation` (TS `!cmd.disableModelInvocation`).
    #[test]
    fn model_invocable_commands_excludes_disabled() {
        let mut reg = CommandRegistry::new();
        reg.register_command(markdown_cmd("visible", vec![]));

        let mut hidden = markdown_cmd("hidden", vec![]);
        hidden.disable_model_invocation = true;
        reg.register_command(hidden);

        let invocable: Vec<&str> = reg
            .model_invocable_commands()
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(invocable.contains(&"visible"));
        assert!(!invocable.contains(&"hidden"));

        // `list_all` still includes both.
        assert_eq!(reg.list_all().len(), 2);
    }

    #[test]
    fn session_skill_allowlist_matches_native_286_rx() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tools/shell/tests/fixtures/bash_precommit_286.json"
        ))
        .unwrap();
        for case in fixture["allowlist_cases"].as_array().unwrap() {
            let mut registry = CommandRegistry::new();
            for input in case["commands"].as_array().unwrap() {
                let mut command = markdown_cmd(
                    input["name"].as_str().unwrap(),
                    input["aliases"]
                        .as_array()
                        .map_or_else(Vec::new, |aliases| {
                            aliases
                                .iter()
                                .map(|value| value.as_str().unwrap().to_string())
                                .collect()
                        }),
                );
                command.source = match input["source"].as_str().unwrap() {
                    "plugin" => CommandSource::Plugin,
                    "mcp" => CommandSource::Mcp,
                    "settings" => CommandSource::Settings(lingxi_core::types::SettingsScope::User),
                    other => panic!("unknown oracle source {other}"),
                };
                // `markdown_cmd` is an alias-test helper with a builtin kind.
                // RX tests `type === "prompt"` before excluding MCP suffixes,
                // so the differential adapter must preserve the oracle's type.
                assert_eq!(input["type"].as_str(), Some("prompt"));
                command.kind = SlashCommandKind::Markdown {
                    file_path: std::path::PathBuf::from("native-rx-fixture.md"),
                    frontmatter: crate::model::CommandFrontmatter::default(),
                    prompt_template: String::new(),
                };
                command.loaded_from = input["loadedFrom"].as_str().map(str::to_string);
                registry.register_command(command);
            }
            registry.set_session_skill_allowlist(case["allowlist"].as_array().map(|names| {
                names
                    .iter()
                    .map(|value| value.as_str().unwrap().to_string())
                    .collect()
            }));
            let mut selected: Vec<_> = registry
                .model_invocable_commands()
                .iter()
                .map(|command| command.name.clone())
                .collect();
            selected.sort();
            let mut expected: Vec<_> = case["selected"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap().to_string())
                .collect();
            expected.sort();
            assert_eq!(
                selected, expected,
                "native RX with allowlist {}",
                case["allowlist"]
            );
            assert!(
                registry.resolve("verify").is_some(),
                "manual slash commands remain available"
            );
        }
    }

    /// A `SlashCommand::default()` leaves every new metadata field unset.
    #[test]
    fn default_slash_command_has_defaulted_metadata() {
        let c = SlashCommand::default();
        assert!(!c.disable_model_invocation);
        assert!(!c.has_user_specified_description);
        assert!(c.loaded_from.is_none());
        assert!(c.when_to_use.is_none());
        assert!(c.aliases.is_empty());
        assert!(c.argument_hint.is_none());
        assert!(c.skill_root.is_none());
        assert!(c.user_invocable.is_none());
        assert!(c.content_length.is_none());
        assert!(c.menu_description.is_none());
    }

    #[test]
    fn unregister_loaded_from_removes_commands_and_aliases() {
        let mut reg = CommandRegistry::new();
        let mut skill = markdown_cmd("review", vec!["rv".to_string()]);
        skill.loaded_from = Some("skills".to_string());
        reg.register_command(skill);
        reg.register_command(markdown_cmd("keep", vec!["k".to_string()]));

        assert_eq!(reg.unregister_loaded_from("skills"), 1);
        assert!(reg.resolve("review").is_none());
        assert!(reg.resolve("rv").is_none());
        assert!(reg.resolve("keep").is_some());
        assert!(reg.resolve("k").is_some());
    }

    #[test]
    fn disk_reload_cannot_replace_or_resurrect_a_reserved_plugin_command() {
        let plugin_id = PluginId::new();
        let mut plugin = markdown_cmd("acme-plugin:frontend-design", vec![]);
        plugin.source = CommandSource::Plugin;
        plugin.kind = SlashCommandKind::Plugin {
            plugin_id,
            file_path: "verified/SKILL.md".into(),
            frontmatter: Default::default(),
            prompt_template: "VERIFIED".into(),
        };
        let mut reg = CommandRegistry::new();
        reg.register_plugin_commands(plugin_id, vec![plugin]);

        let mut decoy = markdown_cmd("acme-plugin:frontend-design", vec![]);
        decoy.loaded_from = Some("skills".into());
        reg.register_command(decoy.clone());
        assert_eq!(
            reg.resolve("acme-plugin:frontend-design")
                .map(|command| command.source),
            Some(CommandSource::Plugin)
        );

        reg.unregister_plugin(&plugin_id);
        reg.register_command(decoy);
        assert_eq!(reg.unregister_non_plugin_prefix("acme-plugin:"), 1);
        assert!(reg.resolve("acme-plugin:frontend-design").is_none());
    }

    /// `get_handler` canonicalizes through the aliases map (mirroring `resolve`),
    /// so an alias (`continue` → `resume`) returns the target's handler while a
    /// non-alias name remains a plain lookup and an unknown name yields `None`.
    #[test]
    fn get_handler_follows_alias() {
        let mut reg = CommandRegistry::new();
        reg.register_builtin_handler(Arc::new(UnimplementedCommandHandler::new(
            "resume",
            "Resume a previous conversation",
        )));
        reg.register_alias("continue".to_string(), "resume".to_string());

        // The alias resolves to the resume handler.
        let via_alias = reg
            .get_handler("continue")
            .expect("alias should map to the resume handler");
        assert_eq!(via_alias.name(), "resume");

        // The canonical name still resolves directly (non-alias no-op path).
        let direct = reg
            .get_handler("resume")
            .expect("canonical name should resolve");
        assert_eq!(direct.name(), "resume");

        // Canonicalization is a no-op for an unknown, non-alias name.
        assert!(reg.get_handler("nonexistent").is_none());
    }

    #[test]
    fn taking_builtin_handlers_preserves_catalog_and_drops_outside_owner() {
        let mut reg = CommandRegistry::new();
        let handler = Arc::new(UnimplementedCommandHandler::new(
            "resume",
            "Resume a previous conversation",
        ));
        let weak = Arc::downgrade(&handler);
        reg.register_builtin_handler(handler.clone());
        reg.register_alias("continue".to_string(), "resume".to_string());
        drop(handler);

        let retired = reg.take_builtin_handlers();
        assert!(reg.get_handler("resume").is_none());
        assert!(reg.get_handler("continue").is_none());
        assert_eq!(
            reg.resolve("continue").map(|command| command.name.as_str()),
            Some("resume"),
            "shutdown must retain command and alias metadata"
        );
        assert!(weak.upgrade().is_some(), "the caller owns retired handlers");

        drop(retired);
        assert!(
            weak.upgrade().is_none(),
            "retired handler destruction is controlled by the caller"
        );
    }

    #[test]
    fn aliases_for_includes_direct_and_registered_aliases() {
        let mut reg = CommandRegistry::new();
        reg.register_command(markdown_cmd("resume", vec!["continue".to_string()]));
        reg.register_alias("unpause".to_string(), "resume".to_string());

        assert_eq!(
            reg.aliases_for("resume"),
            vec!["continue".to_string(), "unpause".to_string()]
        );
    }

    #[test]
    fn palette_commands_include_manual_only_but_filter_non_user_and_env_disabled() {
        let _guard = crate::builtin_support::names::ENV_LOCK.lock().unwrap();
        let mut reg = CommandRegistry::new();

        let mut manual_only = markdown_cmd("manual-only", vec![]);
        manual_only.disable_model_invocation = true;
        reg.register_command(manual_only);

        let mut hidden_from_user = markdown_cmd("internal", vec![]);
        hidden_from_user.user_invocable = Some(false);
        reg.register_command(hidden_from_user);

        reg.register_builtin_handler(Arc::new(UnimplementedCommandHandler::new("login", "Login")));

        std::env::set_var("DISABLE_LOGIN_COMMAND", "1");
        let palette: Vec<String> = reg
            .palette_commands()
            .into_iter()
            .map(|command| command.name)
            .collect();
        std::env::remove_var("DISABLE_LOGIN_COMMAND");

        assert!(palette.contains(&"manual-only".to_string()));
        assert!(!palette.contains(&"internal".to_string()));
        assert!(!palette.contains(&"login".to_string()));
    }
}
