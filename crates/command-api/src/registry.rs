//! In-memory registry of slash commands with alias and plugin-scoped lookup.

use crate::model::{BuiltinCommandHandler, CommandSource, SlashCommand, SlashCommandKind};
use protocol::PluginId;
use std::collections::HashMap;
use std::sync::Arc;

/// Owns all known slash commands and built-in handler implementations.
pub struct CommandRegistry {
    commands: HashMap<String, SlashCommand>,
    aliases: HashMap<String, String>,
    builtin_handlers: HashMap<String, Arc<dyn BuiltinCommandHandler>>,
    plugin_commands: HashMap<PluginId, Vec<String>>,
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
        }
    }

    /// Insert a fully-formed [`SlashCommand`] (used for markdown / plugin / MCP
    /// entries), indexing every entry in `cmd.aliases` so the command resolves by
    /// any of its alternate names. Mirrors the TS `findCommand` search over
    /// `name` + `aliases` (`claude-code/src/commands.ts:690`).
    /// Answers: may a later load replace an already-registered command.
    ///
    /// One of several orderings over these rungs; `protocol::scope`'s module docs index them all and say which question each answers.
    pub fn register_command(&mut self, cmd: SlashCommand) {
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
            self.aliases.insert(alias.clone(), target.to_string());
        }
    }

    /// Register a Rust-side built-in handler under its `name()`.
    pub fn register_builtin_handler(&mut self, h: Arc<dyn BuiltinCommandHandler>) {
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
        let names: Vec<String> = cmds.iter().map(|c| c.name.clone()).collect();
        for c in cmds {
            self.index_aliases(&c.name, &c.aliases);
            self.commands.insert(c.name.clone(), c);
        }
        self.plugin_commands.insert(plugin_id, names);
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
            .collect()
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
            .filter(|(_, command)| command.loaded_from.as_deref() == Some(loaded_from))
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
            source: CommandSource::Settings(protocol::SettingsScope::Project),
            kind: SlashCommandKind::Builtin {
                handler_id: name.to_string(),
            },
            aliases,
            ..SlashCommand::default()
        }
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
        let mut plugin = markdown_cmd("lingxi-local-app:frontend-design", vec![]);
        plugin.source = CommandSource::Plugin;
        plugin.kind = SlashCommandKind::Plugin {
            plugin_id,
            file_path: "verified/SKILL.md".into(),
            frontmatter: Default::default(),
            prompt_template: "VERIFIED".into(),
        };
        let mut reg = CommandRegistry::new();
        reg.register_plugin_commands(plugin_id, vec![plugin]);

        let mut decoy = markdown_cmd("lingxi-local-app:frontend-design", vec![]);
        decoy.loaded_from = Some("skills".into());
        reg.register_command(decoy.clone());
        assert_eq!(
            reg.resolve("lingxi-local-app:frontend-design")
                .map(|command| command.source),
            Some(CommandSource::Plugin)
        );

        reg.unregister_plugin(&plugin_id);
        reg.register_command(decoy);
        assert_eq!(reg.unregister_non_plugin_prefix("lingxi-local-app:"), 1);
        assert!(reg.resolve("lingxi-local-app:frontend-design").is_none());
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
