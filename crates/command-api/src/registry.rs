//! In-memory registry of slash commands with alias and plugin-scoped lookup.

use crate::model::{BuiltinCommandHandler, CommandSource, SlashCommand, SlashCommandKind};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::PluginId;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[cfg(test)]
use serde_json::{json, Value};

/// Owns all known slash commands and built-in handler implementations.
pub struct CommandRegistry {
    commands: HashMap<Vec<u16>, SlashCommand>,
    /// Registration order is the headless command source order. UI consumers
    /// can continue to sort their own projection without changing it.
    registration_order: Vec<Vec<u16>>,
    aliases: HashMap<Vec<u16>, Vec<u16>>,
    builtin_handlers: HashMap<Vec<u16>, Arc<dyn BuiltinCommandHandler>>,
    plugin_commands: HashMap<PluginId, Vec<Vec<u16>>>,
    session_skill_allowlist: Option<Vec<String>>,
    /// Exact plugin display-name units -> (owner id, storage identity, native provider).
    plugin_describe_providers: HashMap<Vec<u16>, (PluginId, String, Utf16JsonProjection)>,
    managed_mcp_servers: HashSet<Vec<u16>>,
    mod_commands: HashMap<Vec<u16>, (String, bool)>,
}

impl CommandRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            commands: HashMap::new(),
            registration_order: Vec::new(),
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
    pub fn register_command(&mut self, mut cmd: SlashCommand) {
        let Some(name_units) = cmd.name.string_units("") else {
            return;
        };
        if name_units.is_empty() {
            return;
        }
        if self.mod_commands.contains_key(&name_units) {
            return;
        }
        // Plugin names are qualified namespaces. A later project/user reload
        // must not replace the live owner behind `plugin:skill`; otherwise a
        // disk skill can impersonate a verified Plugin until the next boot.
        if self.commands.get(&name_units).is_some_and(|current| {
            current.source == CommandSource::Plugin && cmd.source != CommandSource::Plugin
        }) {
            return;
        }
        if matches!(
            cmd.kind,
            SlashCommandKind::Markdown { .. }
                | SlashCommandKind::Plugin { .. }
                | SlashCommandKind::Mcp { .. }
        ) {
            cmd.is_prompt = true;
        }
        self.index_aliases(&name_units, &cmd.aliases);
        self.insert_command(name_units, cmd);
    }

    fn insert_command(&mut self, name_units: Vec<u16>, command: SlashCommand) {
        if !self.commands.contains_key(&name_units) {
            self.registration_order.push(name_units.clone());
        }
        self.commands.insert(name_units, command);
    }

    fn remove_command(&mut self, name_units: &[u16]) {
        self.commands.remove(name_units);
        self.registration_order
            .retain(|registered| registered.as_slice() != name_units);
    }

    /// Point each alias at `target` in the alias map (used by
    /// [`Self::register_command`] / [`Self::register_plugin_commands`]).
    fn index_aliases(&mut self, target: &[u16], aliases: &[Utf16JsonProjection]) {
        for alias in aliases {
            let Some(alias_units) = alias.string_units("") else {
                continue;
            };
            if alias_units.is_empty() {
                continue;
            }
            if self.mod_commands.contains_key(&alias_units) {
                continue;
            }
            self.aliases.insert(alias_units, target.to_vec());
        }
    }

    /// Register a Rust-side built-in handler under its `name()`.
    pub fn register_builtin_handler(&mut self, h: Arc<dyn BuiltinCommandHandler>) {
        let name = h.name().to_string();
        let name_units = name.encode_utf16().collect::<Vec<_>>();
        if self.mod_commands.contains_key(&name_units) {
            return;
        }
        let cmd = SlashCommand {
            name: name.clone().into(),
            description: h.description().into(),
            argument_hint: h.argument_hint().map(Into::into),
            source: CommandSource::Builtin,
            kind: SlashCommandKind::Builtin {
                handler_id: name.clone(),
            },
            is_hidden: h.is_hidden(),
            is_enabled: h.is_enabled(),
            is_prompt: h.is_prompt_command(),
            disable_model_invocation: h.disable_model_invocation(),
            ..SlashCommand::default()
        };
        self.insert_command(name_units.clone(), cmd);
        self.builtin_handlers.insert(name_units, h);
    }

    /// Make the exact UTF-16 `alias` resolve to the exact UTF-16 `target`.
    pub fn register_alias(&mut self, alias: Utf16JsonProjection, target: Utf16JsonProjection) {
        let (Some(alias_units), Some(target_units)) =
            (alias.string_units(""), target.string_units(""))
        else {
            return;
        };
        if alias_units.is_empty() || target_units.is_empty() {
            return;
        }
        if self.mod_commands.contains_key(&alias_units) {
            return;
        }
        self.aliases.insert(alias_units, target_units);
    }

    /// Resolve an exact source name (possibly an alias) to a registered command.
    #[must_use]
    pub fn resolve(&self, name: &Utf16JsonProjection) -> Option<&SlashCommand> {
        let name_units = name.string_units("")?;
        if let Some(command) = self.commands.get(&name_units) {
            return Some(command);
        }
        let canon = self.aliases.get(&name_units).unwrap_or(&name_units);
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
    pub fn get_handler(
        &self,
        handler_id: &Utf16JsonProjection,
    ) -> Option<Arc<dyn BuiltinCommandHandler>> {
        let handler_units = handler_id.string_units("")?;
        if let Some(handler) = self.builtin_handlers.get(&handler_units) {
            return Some(handler.clone());
        }
        let canon = self.aliases.get(&handler_units).unwrap_or(&handler_units);
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
        let names: Vec<Vec<u16>> = cmds
            .iter()
            .filter_map(|command| {
                command
                    .name
                    .string_units("")
                    .filter(|name| !name.is_empty())
            })
            .filter(|name| !self.mod_commands.contains_key(name))
            .collect();
        for mut command in cmds {
            let Some(name_units) = command.name.string_units("") else {
                continue;
            };
            if name_units.is_empty() {
                continue;
            }
            if self.mod_commands.contains_key(&name_units) {
                continue;
            }
            if matches!(
                command.kind,
                SlashCommandKind::Markdown { .. }
                    | SlashCommandKind::Plugin { .. }
                    | SlashCommandKind::Mcp { .. }
            ) {
                command.is_prompt = true;
            }
            self.index_aliases(&name_units, &command.aliases);
            self.insert_command(name_units, command);
        }
        self.plugin_commands.insert(plugin_id, names);
    }

    /// Owner of a command registered by a Mod, if any.
    #[must_use]
    pub fn mod_owner_of(&self, name: &Utf16JsonProjection) -> Option<&str> {
        self.mod_commands
            .get(&name.string_units("")?)
            .map(|(owner, _)| owner.as_str())
    }

    /// Whether a live Mod command executes immediately when selected.
    #[must_use]
    pub fn mod_immediate_of(&self, name: &Utf16JsonProjection) -> Option<bool> {
        self.mod_commands
            .get(&name.string_units("")?)
            .map(|(_, immediate)| *immediate)
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
            display_name.encode_utf16().collect(),
            (
                plugin_id,
                storage_identity.to_owned(),
                provider_projection(string_projection(storage_identity), tier),
            ),
        );
    }

    fn plugin_describe_provider(&self, name: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
        self.plugin_describe_providers
            .get(&string_units(name))
            .map(|(_, _, provider)| provider.clone())
    }

    /// Preserve the native `enterprise`/`managed` MCP authority distinction
    /// for prompt commands contributed by this connected server.
    pub fn set_mcp_server_describe_scope(&mut self, server: &Utf16JsonProjection, managed: bool) {
        let Some(server_units) = server.string_units("") else {
            return;
        };
        if server_units.is_empty() {
            return;
        }
        if managed {
            self.managed_mcp_servers.insert(server_units);
        } else {
            self.managed_mcp_servers.remove(&server_units);
        }
    }

    /// Native `command.describe` origin for a catalog entry. A registered Mod
    /// takes precedence over the command's synthetic Builtin dispatch kind.
    #[must_use]
    pub fn mod_describe_provider(&self, command: &SlashCommand) -> Utf16JsonProjection {
        if let Some(owner) = self.mod_owner_of(&command.name) {
            let owner = string_projection(owner);
            return self
                .plugin_describe_provider(&owner)
                .unwrap_or_else(|| provider_projection(owner, "user"));
        }
        match command.source {
            CommandSource::Builtin | CommandSource::Bundled => {
                provider_projection(string_projection("engine"), "core")
            }
            CommandSource::Settings(lingxi_core::types::SettingsScope::Managed) => {
                provider_projection(string_projection("policy"), "prepend")
            }
            CommandSource::Settings(scope) => {
                let plugin = match scope {
                    lingxi_core::types::SettingsScope::User => "user",
                    lingxi_core::types::SettingsScope::Project => "project",
                    lingxi_core::types::SettingsScope::Local => "local",
                    lingxi_core::types::SettingsScope::Managed => unreachable!(),
                };
                provider_projection(string_projection(plugin), "user")
            }
            CommandSource::Plugin => {
                let owner = split_name(&command.name, b':', false)
                    .map(|(plugin, _)| projection_from_units(plugin))
                    .unwrap_or_else(|| string_projection(""));
                self.plugin_describe_provider(&owner)
                    .unwrap_or_else(|| provider_projection(owner, "user"))
            }
            CommandSource::Mcp => {
                let server = mcp_server_name(command).unwrap_or_else(|| string_projection(""));
                if self.managed_mcp_servers.contains(&string_units(&server)) {
                    return provider_projection(prefix_projection("mcp:", &server), "prepend");
                }
                if let Some(owner) = source_plugin_owner(&server) {
                    if let Some(provider) = self.plugin_describe_provider(&owner) {
                        return provider;
                    }
                }
                provider_projection(prefix_projection("mcp:", &server), "user")
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
        let Some(name_units) = command.name.string_units("") else {
            return;
        };
        if name_units.is_empty() {
            return;
        }
        self.aliases.remove(&name_units);
        self.mod_commands
            .insert(name_units.clone(), (owner.to_owned(), immediate));
        self.builtin_handlers.insert(name_units.clone(), handler);
        self.insert_command(name_units, command);
    }

    /// Remove the commands served by one unloaded Mod.
    pub fn unregister_mod_owner(&mut self, owner: &str) {
        let display_owner = self
            .plugin_describe_providers
            .iter()
            .find(|(_, (_, identity, _))| identity == owner)
            .map_or_else(
                || owner.to_owned(),
                |(name, _)| {
                    String::from_utf16(name)
                        .expect("plugin display names originate from valid Rust strings")
                },
            );
        let names = self
            .mod_commands
            .iter()
            .filter(|(_, (registered_owner, _))| registered_owner == &display_owner)
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for name in names {
            self.mod_commands.remove(&name);
            self.remove_command(&name);
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

    /// Registered commands in their source/assembly order.
    #[must_use]
    pub fn registration_ordered_commands(&self) -> Vec<&SlashCommand> {
        self.registration_order
            .iter()
            .filter_map(|name| self.commands.get(name))
            .collect()
    }

    /// Build the Native headless unknown-command catalog. The availability
    /// count is measured after Native visibility/enabled gates but before the
    /// prompt-type and `disableModelInvocation` gates used for the names list.
    #[must_use]
    pub fn headless_command_catalog(&self) -> crate::model::HeadlessCommandCatalog {
        let available = self
            .registration_ordered_commands()
            .into_iter()
            .filter(|command| self.native_headless_command_available(command))
            .collect::<Vec<_>>();
        // Native assembles prompt skills before handler-backed prompt
        // commands. Keep each group's registry order while placing prompt
        // builtins after the skill/plugin producers that register later in
        // LingXi's startup sequence.
        let prompt_command_names = available
            .iter()
            .filter(|command| {
                native_prompt_model_invocable(command)
                    && !matches!(command.kind, SlashCommandKind::Builtin { .. })
            })
            .chain(available.iter().filter(|command| {
                native_prompt_model_invocable(command)
                    && matches!(command.kind, SlashCommandKind::Builtin { .. })
            }))
            .map(|command| command.name.clone())
            .collect();
        crate::model::HeadlessCommandCatalog {
            prompt_command_names,
            available_command_count: available.len(),
        }
    }

    fn native_headless_command_available(&self, command: &SlashCommand) -> bool {
        if command.is_hidden
            || !command.is_enabled
            || command.user_invocable == Some(false)
            || !self.session_skill_allowed(command)
        {
            return false;
        }
        if matches!(command.kind, SlashCommandKind::Builtin { .. }) {
            if let Some(name) = projection_text(&command.name) {
                if crate::builtin_support::names::is_palette_hidden(name)
                    || crate::builtin_support::names::is_command_env_disabled(name)
                {
                    return false;
                }
            }
        }
        true
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
        let command_units = string_units(&command.name);
        names.iter().any(|name| {
            let name_units = name.encode_utf16().collect::<Vec<_>>();
            command_units == name_units
                || if command.loaded_from.as_deref() == Some("syncedSkills") {
                    command
                        .aliases
                        .iter()
                        .any(|alias| string_units(alias) == name_units)
                } else {
                    let prompt = matches!(
                        command.kind,
                        SlashCommandKind::Markdown { .. }
                            | SlashCommandKind::Plugin { .. }
                            | SlashCommandKind::Bundled { .. }
                            | SlashCommandKind::Mcp { .. }
                    );
                    let mut suffix = vec![b':' as u16];
                    suffix.extend(name_units);
                    !(prompt && command.source == CommandSource::Mcp)
                        && command_units.ends_with(&suffix)
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
                    || projection_text(&command.name).is_none_or(|name| {
                        !crate::builtin_support::names::is_command_env_disabled(name)
                    })
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
    pub fn aliases_for(&self, target: &Utf16JsonProjection) -> Vec<Utf16JsonProjection> {
        let target_units = string_units(target);
        let mut aliases = self
            .resolve(target)
            .map_or_else(Vec::new, |command| command.aliases.clone());
        if let Some(target_text) = projection_text(target) {
            for alias in crate::builtin_support::names::command_aliases(target_text) {
                let projection: Utf16JsonProjection = (*alias).into();
                if !aliases
                    .iter()
                    .any(|existing| string_units(existing) == string_units(&projection))
                {
                    aliases.push(projection);
                }
            }
        }
        for (alias, canonical) in &self.aliases {
            if canonical == &target_units
                && !aliases
                    .iter()
                    .any(|existing| string_units(existing) == *alias)
            {
                aliases.push(projection_from_units(alias.clone()));
            }
        }
        aliases.sort_by_key(string_units);
        aliases
    }

    /// Remove every command previously registered under `plugin_id`, along with
    /// any aliases that pointed at those commands.
    pub fn unregister_plugin(&mut self, plugin_id: &PluginId) {
        if let Some(names) = self.plugin_commands.remove(plugin_id) {
            for n in &names {
                self.remove_command(n);
            }
            self.aliases.retain(|_, target| !names.contains(target));
        }
        self.plugin_describe_providers
            .retain(|_, (owner_id, _, _)| owner_id != plugin_id);
    }

    /// Remove only the prompt commands advertised by one disconnected MCP
    /// server, leaving same-prefix local and plugin commands intact.
    pub fn unregister_mcp_server_prompts(&mut self, server: &Utf16JsonProjection) {
        let Some(mut prefix) = server.string_units("") else {
            return;
        };
        prefix.push(b':' as u16);
        let names = self
            .commands
            .iter()
            .filter(|(_, command)| {
                command.source == CommandSource::Mcp
                    && command.aliases.iter().any(|alias| {
                        alias
                            .string_units("")
                            .is_some_and(|alias| alias.starts_with(&prefix))
                    })
            })
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for name in &names {
            self.remove_command(name);
        }
        self.aliases.retain(|_, target| !names.contains(target));
        if let Some(server_units) = server.string_units("") {
            self.managed_mcp_servers.remove(&server_units);
        }
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
            self.remove_command(name);
        }
        self.aliases.retain(|_, target| !old_names.contains(target));
        self.managed_mcp_servers.clear();
        for (command, managed) in prompts {
            if let Some(server) = mcp_server_name(&command) {
                self.set_mcp_server_describe_scope(&server, managed);
            }
            let local_name_collision = std::iter::once(&command.name)
                .chain(command.aliases.iter())
                .any(|name| {
                    self.resolve(name)
                        .is_some_and(|existing| existing.source != CommandSource::Mcp)
                });
            if !local_name_collision {
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
        let names: Vec<Vec<u16>> = self
            .commands
            .iter()
            .filter(|(name, command)| {
                !self.mod_commands.contains_key(*name)
                    && command.loaded_from.as_deref() == Some(loaded_from)
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in &names {
            self.remove_command(name);
        }
        self.aliases.retain(|_, target| !names.contains(target));
        names.len()
    }

    /// Remove non-Plugin commands attempting to occupy a reserved Plugin
    /// namespace. Hosts use this after disk reload so a disabled builtin
    /// plugin cannot be impersonated by a same-name project skill.
    pub fn unregister_non_plugin_prefix(&mut self, prefix: &str) -> usize {
        let prefix_units = prefix.encode_utf16().collect::<Vec<_>>();
        let names: Vec<Vec<u16>> = self
            .commands
            .iter()
            .filter(|(name, command)| {
                name.starts_with(&prefix_units) && command.source != CommandSource::Plugin
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in &names {
            self.remove_command(name);
        }
        self.aliases.retain(|_, target| !names.contains(target));
        names.len()
    }
}

fn native_prompt_model_invocable(command: &&SlashCommand) -> bool {
    command.is_prompt && !command.disable_model_invocation
}

fn string_units(value: &Utf16JsonProjection) -> Vec<u16> {
    value.string_units("").unwrap_or_default()
}

fn projection_from_units(units: Vec<u16>) -> Utf16JsonProjection {
    let display = String::from_utf16_lossy(&units);
    Utf16JsonProjection::root_string(display, units)
        .expect("display projection is derived from the same UTF-16 units")
}

fn projection_text(value: &Utf16JsonProjection) -> Option<&str> {
    let text = value.string_at("")?;
    value
        .string_units("")?
        .iter()
        .copied()
        .eq(text.encode_utf16())
        .then_some(text)
}

fn string_projection(value: &str) -> Utf16JsonProjection {
    value.into()
}

fn prefix_projection(prefix: &str, source: &Utf16JsonProjection) -> Utf16JsonProjection {
    let mut units = prefix.encode_utf16().collect::<Vec<_>>();
    units.extend(string_units(source));
    projection_from_units(units)
}

fn provider_projection(plugin: Utf16JsonProjection, tier: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::object_units(vec![
        ("plugin".encode_utf16().collect(), plugin),
        ("tier".encode_utf16().collect(), string_projection(tier)),
    ])
    .expect("static provider fields form a valid JSON object")
}

fn source_plugin_owner(server: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
    let units = string_units(server);
    let prefix = "plugin:".encode_utf16().collect::<Vec<_>>();
    if !units.starts_with(&prefix) {
        return None;
    }
    let owner_start = prefix.len();
    let owner_end = units[owner_start..]
        .iter()
        .position(|unit| *unit == b':' as u16)
        .map_or(units.len(), |offset| owner_start + offset);
    (owner_start < owner_end).then(|| projection_from_units(units[owner_start..owner_end].to_vec()))
}

/// Recover the exact MCP server name from its source alias and prompt name.
/// Canonical command names are sanitized for Native's `mcp__...__...` route,
/// so they cannot be used as the source of server identity.
fn mcp_server_name(command: &SlashCommand) -> Option<Utf16JsonProjection> {
    let SlashCommandKind::Mcp { prompt_name, .. } = &command.kind else {
        return None;
    };
    let mut suffix = vec![b':' as u16];
    suffix.extend(prompt_name.string_units("")?);
    command.aliases.iter().find_map(|alias| {
        let alias_units = alias.string_units("")?;
        let split = alias_units.len().checked_sub(suffix.len())?;
        (alias_units[split..] == suffix)
            .then(|| projection_from_units(alias_units[..split].to_vec()))
    })
}

#[cfg(test)]
fn provider_value(provider: &Utf16JsonProjection) -> Value {
    serde_json::from_str(
        &provider
            .to_json_string()
            .expect("provider projection is valid JSON"),
    )
    .expect("provider projection serializes as JSON")
}

fn split_name(
    name: &Utf16JsonProjection,
    separator: u8,
    from_right: bool,
) -> Option<(Vec<u16>, Vec<u16>)> {
    let units = string_units(name);
    let separator = u16::from(separator);
    let index = if from_right {
        units.iter().rposition(|unit| *unit == separator)?
    } else {
        units.iter().position(|unit| *unit == separator)?
    };
    Some((units[..index].to_vec(), units[index + 1..].to_vec()))
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
    use crate::mcp_prompts::{mcp_prompt_command_aliases, mcp_prompt_command_name};
    use crate::model::{CommandSource, SlashCommandKind};

    fn markdown_cmd(name: &str, aliases: Vec<String>) -> SlashCommand {
        SlashCommand {
            name: name.into(),
            description: format!("{name} cmd").into(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            kind: SlashCommandKind::Builtin {
                handler_id: name.to_string(),
            },
            aliases: aliases.into_iter().map(Into::into).collect(),
            ..SlashCommand::default()
        }
    }

    fn mcp_prompt(server: &str, prompt: &str) -> SlashCommand {
        let prompt_name = Utf16JsonProjection::from(prompt);
        SlashCommand {
            name: mcp_prompt_command_name(server, &prompt_name),
            aliases: mcp_prompt_command_aliases(server, &prompt_name),
            source: CommandSource::Mcp,
            kind: SlashCommandKind::Mcp {
                connection_id: lingxi_core::types::McpConnectionId::new(),
                prompt_name,
                arguments: Vec::new(),
            },
            ..SlashCommand::default()
        }
    }

    #[test]
    fn headless_catalog_preserves_registration_order_and_native_gates() {
        let mut registry = CommandRegistry::new();
        let prompt = |name: &str| SlashCommand {
            name: name.into(),
            source: CommandSource::Bundled,
            kind: SlashCommandKind::Bundled {
                frontmatter: Default::default(),
                prompt_fn: None,
            },
            ..SlashCommand::default()
        };

        let mut first = prompt("first-prompt");
        first.is_prompt = true;
        registry.register_command(first);
        registry.register_command(SlashCommand {
            name: "local-command".into(),
            source: CommandSource::Builtin,
            kind: SlashCommandKind::Builtin {
                handler_id: "local-command".into(),
            },
            ..SlashCommand::default()
        });
        let mut manual = prompt("manual-prompt");
        manual.is_prompt = true;
        manual.disable_model_invocation = true;
        registry.register_command(manual);
        let mut hidden = prompt("hidden-prompt");
        hidden.is_prompt = true;
        hidden.is_hidden = true;
        registry.register_command(hidden);
        let mut unavailable = prompt("unavailable-prompt");
        unavailable.is_prompt = true;
        unavailable.is_enabled = false;
        registry.register_command(unavailable);
        let mut non_user_invocable = prompt("non-user-prompt");
        non_user_invocable.is_prompt = true;
        non_user_invocable.user_invocable = Some(false);
        registry.register_command(non_user_invocable);

        // Replacing metadata does not move or duplicate the original entry.
        let mut replacement = prompt("first-prompt");
        replacement.is_prompt = true;
        replacement.description = "replacement".into();
        registry.register_command(replacement);

        let ordered_names = registry
            .registration_ordered_commands()
            .iter()
            .map(|command| command.name.string_at("").unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            ordered_names,
            [
                "first-prompt",
                "local-command",
                "manual-prompt",
                "hidden-prompt",
                "unavailable-prompt",
                "non-user-prompt",
            ]
        );

        let catalog = registry.headless_command_catalog();
        assert_eq!(catalog.available_command_count, 3);
        assert_eq!(
            catalog
                .prompt_command_names
                .iter()
                .map(|name| name.string_at("").unwrap())
                .collect::<Vec<_>>(),
            ["first-prompt"]
        );
    }

    #[test]
    fn unknown_reminder_keeps_source_units_and_native_remainder_count() {
        let unknown = Utf16JsonProjection::root_string(
            "unknown�".into(),
            ["unknown".encode_utf16().collect::<Vec<_>>(), vec![0xd800]].concat(),
        )
        .unwrap();
        let catalog = crate::model::HeadlessCommandCatalog {
            prompt_command_names: vec!["first".into(), "second".into()],
            available_command_count: 5,
        };
        let rendered = crate::model::render_unknown_command_reminder(&unknown, &catalog);
        let expected_prefix =
            "<system-reminder>\nThe user's message starts with the slash command /unknown"
                .encode_utf16()
                .collect::<Vec<_>>();
        let rendered_units = rendered.string_units("").unwrap();
        assert!(rendered_units.starts_with(&expected_prefix));
        assert_eq!(
            rendered_units[expected_prefix.len()],
            0xd800,
            "the unknown name's source unit is preserved"
        );
        assert!(rendered.string_at("").unwrap().contains("and 3 more."));
    }

    #[test]
    fn unknown_reminder_lists_at_most_forty_names_in_source_order() {
        let catalog = crate::model::HeadlessCommandCatalog {
            prompt_command_names: (0..41)
                .map(|index| format!("prompt-{index}").into())
                .collect(),
            available_command_count: 42,
        };
        let rendered = crate::model::render_unknown_command_reminder(&"missing".into(), &catalog);
        let text = rendered.string_at("").unwrap();
        assert!(text.contains("/prompt-0, /prompt-1"));
        assert!(text.contains("/prompt-39, and 2 more."));
        assert!(!text.contains("/prompt-40"));
    }

    #[test]
    fn command_describe_provider_uses_native_source_tiers_and_mod_owner() {
        let mut registry = CommandRegistry::new();
        let project = markdown_cmd("deploy", Vec::new());
        assert_eq!(
            provider_value(&registry.mod_describe_provider(&project)),
            json!({"plugin":"project","tier":"user"})
        );
        let managed = SlashCommand {
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Managed),
            ..project.clone()
        };
        assert_eq!(
            provider_value(&registry.mod_describe_provider(&managed)),
            json!({"plugin":"policy","tier":"prepend"})
        );
        let mcp = mcp_prompt("build", "run");
        assert_eq!(
            provider_value(&registry.mod_describe_provider(&mcp)),
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
        assert_eq!(registry.mod_immediate_of(&"custom".into()), Some(true));
        assert_eq!(
            provider_value(&registry.mod_describe_provider(&mod_command)),
            json!({"plugin":"demo","tier":"user"})
        );
    }

    #[test]
    fn command_describe_provider_keeps_exact_mcp_server_units() {
        let mut registry = CommandRegistry::new();
        let server = Utf16JsonProjection::root_string(
            "srv�".into(),
            vec![b's' as u16, b'r' as u16, b'v' as u16, 0xd800],
        )
        .unwrap();
        let prompt_name = Utf16JsonProjection::from("prompt");
        let mut alias_units = string_units(&server);
        alias_units.push(b':' as u16);
        alias_units.extend(prompt_name.string_units("").unwrap_or_default());
        let mut marker_alias_units = alias_units.clone();
        marker_alias_units.extend(" (MCP)".encode_utf16());
        let command = SlashCommand {
            name: "mcp__srv___prompt".into(),
            aliases: vec![
                projection_from_units(alias_units),
                projection_from_units(marker_alias_units),
            ],
            source: CommandSource::Mcp,
            kind: SlashCommandKind::Mcp {
                connection_id: lingxi_core::types::McpConnectionId::new(),
                prompt_name,
                arguments: Vec::new(),
            },
            ..SlashCommand::default()
        };

        registry.set_mcp_server_describe_scope(&server, true);
        let provider = registry.mod_describe_provider(&command);
        let mut expected_plugin_units = "mcp:".encode_utf16().collect::<Vec<_>>();
        expected_plugin_units.extend(string_units(&server));
        assert_eq!(
            provider.string_units("/plugin"),
            Some(expected_plugin_units)
        );
        assert_eq!(provider.string_at("/tier"), Some("prepend"));
        assert_eq!(
            provider.to_json_string().unwrap(),
            r#"{"plugin":"mcp:srv\ud800","tier":"prepend"}"#
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
        let mcp_prompt = mcp_prompt("plugin:demo:server", "prompt");
        for command in [&skill, &mcp_prompt] {
            assert_eq!(
                provider_value(&registry.mod_describe_provider(command)),
                json!({"plugin":"demo@market","tier":"append"})
            );
        }
        registry.set_mcp_server_describe_scope(&"plugin:demo:server".into(), true);
        assert_eq!(
            provider_value(&registry.mod_describe_provider(&mcp_prompt)),
            json!({"plugin":"mcp:plugin:demo:server","tier":"prepend"})
        );
        registry.set_mcp_server_describe_scope(&"plugin:demo:server".into(), false);
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
            provider_value(&registry.mod_describe_provider(&mod_command)),
            json!({"plugin":"demo@market","tier":"append"})
        );
        registry.unregister_mod_owner("demo@market");
        assert!(registry.resolve(&"custom".into()).is_none());
        registry.unregister_plugin(&plugin_id);
        assert_eq!(
            provider_value(&registry.mod_describe_provider(&skill)),
            json!({"plugin":"demo","tier":"user"})
        );
    }

    #[test]
    fn disconnect_removes_only_the_servers_prompt_commands() {
        let mut registry = CommandRegistry::new();
        let prompt = mcp_prompt("team:server", "review");
        let local = SlashCommand {
            name: "team:server:local".into(),
            ..markdown_cmd("local", Vec::new())
        };
        registry.register_command(prompt.clone());
        registry.register_command(local);
        registry.set_mcp_server_describe_scope(&"team:server".into(), true);
        assert_eq!(
            provider_value(&registry.mod_describe_provider(&prompt)),
            json!({"plugin":"mcp:team:server","tier":"prepend"})
        );
        registry.unregister_mcp_server_prompts(&"team:server".into());
        assert!(registry.resolve(&prompt.name).is_none());
        assert!(registry.resolve(&"team:server:review".into()).is_none());
        assert!(registry.resolve(&"team:server:local".into()).is_some());
    }

    #[test]
    fn reconnect_replaces_prompt_partition_and_keeps_local_shadow() {
        let mut registry = CommandRegistry::new();
        let old = mcp_prompt("srv", "old");
        let shadow = markdown_cmd("srv:shadow", Vec::new());
        let aliased_shadow = markdown_cmd("local-shadow", vec!["srv:aliased".into()]);
        registry.register_command(shadow);
        registry.register_command(aliased_shadow);
        registry.reconcile_mcp_prompt_commands(vec![(old.clone(), false)]);
        assert!(registry.resolve(&"srv:old".into()).is_some());
        let replacement = mcp_prompt("srv", "new");
        let remote_shadow = mcp_prompt("srv", "shadow");
        let remote_alias_shadow = mcp_prompt("srv", "aliased");
        registry.reconcile_mcp_prompt_commands(vec![
            (replacement.clone(), true),
            (remote_shadow, true),
            (remote_alias_shadow, true),
        ]);
        assert!(registry.resolve(&"srv:old".into()).is_none());
        assert!(registry.resolve(&"srv:new".into()).is_some());
        assert_eq!(
            registry.resolve(&"srv:shadow".into()).unwrap().source,
            CommandSource::Settings(lingxi_core::types::SettingsScope::Project)
        );
        assert_eq!(
            registry
                .resolve(&"srv:aliased".into())
                .unwrap()
                .name
                .string_at(""),
            Some("local-shadow")
        );
        assert_eq!(
            provider_value(&registry.mod_describe_provider(&replacement)),
            json!({"plugin":"mcp:srv","tier":"prepend"})
        );
        registry.reconcile_mcp_prompt_commands(Vec::new());
        assert!(registry.resolve(&"mcp__srv__new".into()).is_none());
        assert!(registry.resolve(&"srv:shadow".into()).is_some());
        assert!(registry.resolve(&"srv:aliased".into()).is_some());
    }

    /// A command registered with aliases resolves by its canonical name and by
    /// each alias (TS `findCommand` over `name` + `aliases`).
    #[test]
    fn register_command_indexes_each_alias() {
        let mut reg = CommandRegistry::new();
        reg.register_command(markdown_cmd(
            "resume",
            vec!["continue".into(), "unpause".into()],
        ));

        assert_eq!(
            reg.resolve(&"resume".into())
                .and_then(|c| c.name.string_at("")),
            Some("resume")
        );
        assert_eq!(
            reg.resolve(&"continue".into())
                .and_then(|c| c.name.string_at("")),
            Some("resume"),
            "alias `continue` should resolve to `resume`"
        );
        assert_eq!(
            reg.resolve(&"unpause".into())
                .and_then(|c| c.name.string_at("")),
            Some("resume"),
            "alias `unpause` should resolve to `resume`"
        );
        assert!(reg.resolve(&"missing".into()).is_none());
    }

    #[test]
    fn exact_canonical_name_precedes_another_commands_alias() {
        let mut registry = CommandRegistry::new();
        let exact = markdown_cmd("canonical", Vec::new());
        registry.register_command(exact.clone());
        registry.register_command(markdown_cmd("other", vec!["canonical".into()]));

        assert_eq!(
            registry
                .resolve(&"canonical".into())
                .unwrap()
                .name
                .string_at(""),
            exact.name.string_at("")
        );

        registry.register_builtin_handler(Arc::new(UnimplementedCommandHandler::new(
            "resume",
            "resume handler",
        )));
        registry.register_command(markdown_cmd("elsewhere", vec!["resume".into()]));
        assert_eq!(
            registry.get_handler(&"resume".into()).unwrap().name(),
            "resume"
        );
    }

    #[test]
    fn registry_routes_command_and_alias_by_exact_utf16_units() {
        let name = Utf16JsonProjection::root_string(
            "deploy�".into(),
            vec![
                b'd' as u16,
                b'e' as u16,
                b'p' as u16,
                b'l' as u16,
                b'o' as u16,
                b'y' as u16,
                0xd800,
            ],
        )
        .unwrap();
        let alias = Utf16JsonProjection::root_string(
            "ship�".into(),
            vec![b's' as u16, b'h' as u16, b'i' as u16, b'p' as u16, 0xdfff],
        )
        .unwrap();
        let mut command = markdown_cmd("unused", Vec::new());
        command.name = name.clone();
        command.aliases = vec![alias.clone()];

        let mut registry = CommandRegistry::new();
        registry.register_command(command);

        assert_eq!(
            registry.resolve(&alias).unwrap().name.string_units(""),
            name.string_units("")
        );
        assert!(registry.resolve(&"ship�".into()).is_none());
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
            .map(|c| c.name.string_at("").unwrap_or_default())
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
                    prompt_template: String::new().into(),
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
                .map(|command| command.name.string_at("").unwrap_or_default().to_owned())
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
                registry.resolve(&"verify".into()).is_some(),
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
        assert!(reg.resolve(&"review".into()).is_none());
        assert!(reg.resolve(&"rv".into()).is_none());
        assert!(reg.resolve(&"keep".into()).is_some());
        assert!(reg.resolve(&"k".into()).is_some());
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
            reg.resolve(&"acme-plugin:frontend-design".into())
                .map(|command| command.source),
            Some(CommandSource::Plugin)
        );

        reg.unregister_plugin(&plugin_id);
        reg.register_command(decoy);
        assert_eq!(reg.unregister_non_plugin_prefix("acme-plugin:"), 1);
        assert!(reg.resolve(&"acme-plugin:frontend-design".into()).is_none());
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
        reg.register_alias("continue".into(), "resume".into());

        // The alias resolves to the resume handler.
        let via_alias = reg
            .get_handler(&"continue".into())
            .expect("alias should map to the resume handler");
        assert_eq!(via_alias.name(), "resume");

        // The canonical name still resolves directly (non-alias no-op path).
        let direct = reg
            .get_handler(&"resume".into())
            .expect("canonical name should resolve");
        assert_eq!(direct.name(), "resume");

        // Canonicalization is a no-op for an unknown, non-alias name.
        assert!(reg.get_handler(&"nonexistent".into()).is_none());
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
        reg.register_alias("continue".into(), "resume".into());
        drop(handler);

        let retired = reg.take_builtin_handlers();
        assert!(reg.get_handler(&"resume".into()).is_none());
        assert!(reg.get_handler(&"continue".into()).is_none());
        assert_eq!(
            reg.resolve(&"continue".into())
                .and_then(|command| command.name.string_at("")),
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
        reg.register_alias("unpause".into(), "resume".into());

        assert_eq!(
            reg.aliases_for(&"resume".into())
                .iter()
                .map(|alias| alias.string_at("").unwrap_or_default())
                .collect::<Vec<_>>(),
            vec!["continue", "unpause"]
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
            .map(|command| command.name.string_at("").unwrap_or_default().to_owned())
            .collect();
        std::env::remove_var("DISABLE_LOGIN_COMMAND");

        assert!(palette.contains(&"manual-only".to_string()));
        assert!(!palette.contains(&"internal".to_string()));
        assert!(!palette.contains(&"login".to_string()));
    }
}
