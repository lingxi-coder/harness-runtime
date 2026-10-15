//! In-memory registry of slash commands with alias and plugin-scoped lookup.

use crate::model::{
    BuiltinCommandHandler, CommandSource, NativeCommandType, SlashCommand, SlashCommandKind,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::PluginId;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[cfg(test)]
use serde_json::{json, Value};

/// Owns all known slash commands and built-in handler implementations.
pub struct CommandRegistry {
    commands: HashMap<Vec<u16>, SlashCommand>,
    /// Physical registration order, retained for operations whose contract
    /// depends on assembly history.
    registration_order: Vec<Vec<u16>>,
    aliases: HashMap<Vec<u16>, Vec<u16>>,
    builtin_handlers: HashMap<Vec<u16>, Arc<dyn BuiltinCommandHandler>>,
    plugin_commands: HashMap<PluginId, Vec<Vec<u16>>>,
    side_chat: bool,
    session_skill_allowlist: Option<Vec<String>>,
    /// Exact plugin display-name units -> (owner id, storage identity, native provider).
    plugin_describe_providers: HashMap<Vec<u16>, (PluginId, String, Utf16JsonProjection)>,
    managed_mcp_servers: HashSet<Vec<u16>>,
    mod_commands: HashMap<Vec<u16>, (String, bool)>,
    native_headless_context: Option<NativeHeadlessCatalogContext>,
}

/// Host facts used only for Native Headless command-availability predicates.
/// `None` means the caller has not supplied that source fact; it is not
/// interpreted as a rejection. Composition roots that know the launch state
/// should set this before the registry is exposed to the Headless dispatcher.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeHeadlessCatalogContext {
    /// Native CLI entrypoint from the host's current request/session context.
    pub entrypoint: Option<Utf16JsonProjection>,
    /// `true` when this is a child session, used by Native remote-only gates.
    pub is_child_session: Option<bool>,
    /// Whether Native's launch options identify a Claude Code session.
    /// This is independent of the User-Agent `user_type` field.
    pub is_claude_code: Option<bool>,
    /// Current `CLAUDE_CODE_REMOTE` setting, if the host supplied it.
    pub is_remote: Option<bool>,
    /// Main-loop model selected for this session.
    pub selected_model: Option<Utf16JsonProjection>,
    /// Current workspace exists and satisfies the host's workspace-trust rule.
    pub workspace_available: Option<bool>,
    /// Native `insights` policy verdict; absent host facts must stay `None`.
    pub insights_policy_gate: Option<bool>,
    /// Native `tP()` entitlement result for the Local `/usage-credits` twin.
    pub usage_credits_enabled: Option<bool>,
    /// Native `gv()` experimental gate for the Local `/advisor` twin.
    pub advisor_enabled: Option<bool>,
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
            side_chat: false,
            session_skill_allowlist: None,
            plugin_describe_providers: HashMap::new(),
            managed_mcp_servers: HashSet::new(),
            mod_commands: HashMap::new(),
            native_headless_context: None,
        }
    }

    /// Set the request/session facts used by Native Headless factory gates.
    /// This does not affect interactive palette behavior or command ordering.
    pub fn set_native_headless_context(&mut self, context: NativeHeadlessCatalogContext) {
        self.native_headless_context = Some(context);
    }

    /// Restrict this registry instance to lightweight side conversations.
    pub fn set_side_chat(&mut self, enabled: bool) {
        self.side_chat = enabled;
    }

    /// Check Side Chat admission before handlers, hooks, or prompt expansion.
    #[must_use]
    pub fn execution_allowed(&self, command: &SlashCommand) -> bool {
        if !self.side_chat {
            return true;
        }
        if self.mod_owner_of(&command.name).is_some()
            || matches!(command.source, CommandSource::Plugin | CommandSource::Mcp)
        {
            return false;
        }
        match &command.kind {
            SlashCommandKind::Builtin { .. } => matches!(
                projection_text(&command.name),
                Some("help" | "model" | "compact")
            ),
            SlashCommandKind::Markdown { frontmatter, .. }
            | SlashCommandKind::Bundled { frontmatter, .. } => side_skill_allowed(
                &command.name,
                frontmatter.context.as_deref(),
                frontmatter.background,
                frontmatter.agent.as_deref(),
                frontmatter.allowed_tools.as_deref().unwrap_or(&[]),
            ),
            SlashCommandKind::Plugin { .. } | SlashCommandKind::Mcp { .. } => false,
        }
    }

    /// Set a source-bound Native visibility bit on a registered built-in.
    /// Callers use this when the Native command object carries `isHidden` but
    /// the Rust handler trait has no corresponding metadata producer.
    #[cfg(feature = "builtin-handlers")]
    pub(crate) fn set_native_builtin_hidden(&mut self, name: &str, is_hidden: bool) {
        let name_units = name.encode_utf16().collect::<Vec<_>>();
        let Some(command) = self.commands.get_mut(&name_units) else {
            return;
        };
        if command.source == CommandSource::Builtin && command.native_type.is_some() {
            command.is_hidden = is_hidden;
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
                | SlashCommandKind::Bundled { .. }
        ) {
            cmd.native_type = Some(NativeCommandType::Prompt);
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
        let handler_native_type = h.native_type();
        let existing_native = self.commands.get(&name_units);
        let native_type =
            handler_native_type.or_else(|| existing_native.and_then(|command| command.native_type));
        let disable_non_interactive = if handler_native_type.is_some() {
            h.disable_non_interactive()
        } else {
            existing_native.is_some_and(|command| command.disable_non_interactive)
        };
        let supports_non_interactive = if handler_native_type.is_some() {
            h.supports_non_interactive()
        } else {
            existing_native.is_some_and(|command| command.supports_non_interactive)
        };
        // Some desktop handlers replace a prompt facade with a direct local
        // slash handler. When that handler does not provide Native metadata,
        // retain the facade's model-invocation restriction instead of
        // accidentally turning the local extension into a model-invocable
        // prompt.
        let disable_model_invocation = if handler_native_type.is_some() {
            h.disable_model_invocation()
        } else {
            existing_native.map_or_else(
                || h.disable_model_invocation(),
                |command| command.disable_model_invocation,
            )
        };
        let native_source = if handler_native_type.is_some() {
            (native_type == Some(NativeCommandType::Prompt)).then(|| "built-in".into())
        } else {
            existing_native.and_then(|command| command.native_source.clone())
        };
        // A Rust handler without Native command metadata does not replace the
        // visibility property of the source command it implements.
        let is_hidden = if handler_native_type.is_some() {
            h.is_hidden()
        } else {
            existing_native.map_or_else(|| h.is_hidden(), |command| command.is_hidden)
        };
        let cmd = SlashCommand {
            name: name.clone().into(),
            description: h.description().into(),
            argument_hint: h.argument_hint().map(Into::into),
            source: CommandSource::Builtin,
            kind: SlashCommandKind::Builtin {
                handler_id: name.clone(),
            },
            is_hidden,
            is_enabled: h.is_enabled(),
            native_type,
            disable_non_interactive,
            supports_non_interactive,
            native_source,
            disable_model_invocation,
            content_length: h
                .content_length()
                .or_else(|| existing_native.and_then(|command| command.content_length)),
            progress_message: h
                .progress_message()
                .or_else(|| existing_native.and_then(|command| command.progress_message.clone())),
            requires: h
                .requires()
                .or_else(|| existing_native.and_then(|command| command.requires)),
            policy_gate: h
                .policy_gate()
                .or_else(|| existing_native.and_then(|command| command.policy_gate.clone())),
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
                command.native_type = Some(NativeCommandType::Prompt);
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

    /// Registered commands in physical source/assembly order.
    #[must_use]
    pub fn registration_ordered_commands(&self) -> Vec<&SlashCommand> {
        self.registration_order
            .iter()
            .filter_map(|name| self.commands.get(name))
            .collect()
    }

    /// Project the live command pool in Native's source-category order while
    /// preserving registration order inside each category. Native loads
    /// policy, user, and project settings sources before plugin prompts,
    /// bundled skills, and the built-in command table. Project and Local share
    /// the project-stage rank because Native has no distinct local-settings
    /// skill source. Plugin commands precede plugin prompt skills; both are
    /// CommandSource::Plugin, so skill_root distinguishes the factory rows.
    /// MCP prompts are appended as their own source group after that pool.
    #[must_use]
    pub fn native_source_ordered_commands(&self) -> Vec<&SlashCommand> {
        let mut commands = self.registration_ordered_commands();
        commands.sort_by_key(|command| native_command_source_rank(command));
        commands
    }

    /// Build the Native headless unknown-command catalog. Native `bge` first
    /// narrows the command pool by execution type and non-interactive flags;
    /// then `kjs` measures availability before the prompt-type and
    /// `disableModelInvocation` gates used for the names list.
    #[must_use]
    pub fn headless_command_catalog(&self) -> crate::model::HeadlessCommandCatalog {
        let available = self
            .native_source_ordered_commands()
            .into_iter()
            // Native `bge`'s `MPe` admits exactly prompt rows with
            // `!disableNonInteractive` or local rows with
            // `supportsNonInteractive`. Missing `type` is not an admitted
            // Native command row.
            .filter(|command| command.native_non_interactive_admission() == Some(true))
            .filter(|command| self.native_headless_command_available(command))
            .collect::<Vec<_>>();
        let prompt_command_names = available
            .iter()
            .filter(|command| native_prompt_model_invocable(command))
            .map(|command| command.name.clone())
            .collect();
        crate::model::HeadlessCommandCatalog {
            prompt_command_names,
            available_command_count: available.len(),
        }
    }

    fn native_headless_command_available(&self, command: &SlashCommand) -> bool {
        if command.is_hidden
            || !native_factory_enabled(command)
            || !self.session_skill_allowed(command)
            || !self.native_headless_factory_available(command)
        {
            return false;
        }
        true
    }

    fn native_headless_factory_available(&self, command: &SlashCommand) -> bool {
        // `kjs` counts rows in `pe` before applying the headless prompt-name
        // filter. `userInvocable` controls the slash UI and is not a `pe`
        // predicate.
        let Some(name) = projection_text(&command.name) else {
            return true;
        };
        // `version` is always disabled and `extra-usage` always hidden in the
        // Native source. The compiled-in LingXi `/cron` management extension
        // is not a Native 2.1.295 slash command; scope that exclusion to its
        // own Builtin/Bundled source metadata so a user or plugin skill named
        // `/cron` remains in the catalog.
        let local_cron_extension = name == "cron"
            && command.native_source.is_none()
            && matches!(
                command.source,
                CommandSource::Builtin | CommandSource::Bundled
            );
        if matches!(name, "version" | "extra-usage") || local_cron_extension {
            return false;
        }
        if command
            .requires
            .as_ref()
            .and_then(|requires| requires.workspace)
            == Some(true)
            && !self
                .native_headless_context
                .as_ref()
                .is_some_and(|context| context.workspace_available == Some(true))
        {
            return false;
        }
        if let Some(policy_gate) = command.policy_gate.as_ref() {
            let allowed = match policy_gate.policy.as_str() {
                "allow_insights" => self
                    .native_headless_context
                    .as_ref()
                    .is_some_and(|context| context.insights_policy_gate == Some(true)),
                _ => false,
            };
            if !allowed {
                return false;
            }
        }
        let Some(context) = self.native_headless_context.as_ref() else {
            // Preserve unresolved host predicates for commands without an
            // explicit workspace or policy-gate requirement.
            return true;
        };
        match name {
            // 2.1.295 `DNe`: Local + supportsNonInteractive, but hidden and
            // disabled unless the entrypoint is one of Native's remote hosts.
            "add-dir" => context.add_dir_is_enabled() != Some(false),
            // `qFe` Local twin is enabled in ordinary local Headless mode only
            // when a model is configured and the session is not remote.
            "auto-mode-setup" => context.auto_mode_setup_is_enabled() != Some(false),
            // Insights policy is evaluated from its static Native `policyGate`
            // metadata above; do not add a name-based permissive fallback.
            // Native `explain-usage` is only enabled for the remote Cowork
            // entrypoint/session. This Headless host's source-backed
            // `entrypoint` fact distinguishes that route from ordinary
            // `sdk-cli`; the remote Cowork session route is outside this host.
            "explain-usage" => context.explain_usage_is_enabled() != Some(false),
            "usage-credits" => context.usage_credits_enabled != Some(false),
            "advisor" => context.advisor_enabled != Some(false),
            _ => true,
        }
    }

    /// Commands the model is allowed to invoke — every entry whose
    /// `disable_model_invocation` flag is unset. Mirrors the TS filter
    /// `!cmd.disableModelInvocation` used when building the model-facing skill
    /// surface (`claude-code/src/commands.ts:571`).
    #[must_use]
    pub fn model_invocable_commands(&self) -> Vec<&SlashCommand> {
        self.native_source_ordered_commands()
            .into_iter()
            .filter(|c| native_factory_enabled(c))
            .filter(|c| self.native_headless_factory_available(c))
            .filter(|c| !c.disable_model_invocation)
            .filter(|c| self.execution_allowed(c))
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
        if !self.execution_allowed(command) {
            return false;
        }
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
            .filter(|command| native_factory_enabled(command))
            .filter(|command| command.user_invocable != Some(false))
            .filter(|command| self.execution_allowed(command))
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
    command.is_prompt() && !command.disable_model_invocation
}

fn native_command_source_rank(command: &SlashCommand) -> (u8, u8) {
    use lingxi_core::types::SettingsScope;

    match command.source {
        CommandSource::Settings(SettingsScope::Managed) => (0, 0),
        CommandSource::Settings(SettingsScope::User) => (0, 1),
        CommandSource::Settings(SettingsScope::Project | SettingsScope::Local) => (0, 2),
        CommandSource::Plugin => (1, u8::from(command.skill_root.is_some())),
        CommandSource::Bundled => (2, 0),
        CommandSource::Builtin => (3, 0),
        CommandSource::Mcp => (4, 0),
    }
}

pub(crate) fn is_native_builtin_factory(command: &SlashCommand) -> bool {
    matches!(command.kind, SlashCommandKind::Builtin { .. })
        || (command.source == CommandSource::Bundled
            && command
                .native_source
                .as_ref()
                .and_then(|source| source.string_at(""))
                == Some("built-in"))
}

pub(crate) fn native_factory_enabled(command: &SlashCommand) -> bool {
    command.is_enabled
        && (!is_native_builtin_factory(command)
            || projection_text(&command.name)
                .is_none_or(|name| !crate::builtin_support::names::is_command_env_disabled(name)))
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

/// Side Chat omits skills that can fork work, dispatch more agents, or run a
/// detached background task.
#[must_use]
pub fn side_skill_allowed(
    name: &Utf16JsonProjection,
    context: Option<&str>,
    background: Option<bool>,
    agent: Option<&str>,
    allowed_tools: &[String],
) -> bool {
    name.string_units("")
        .is_some_and(|units| !units.into_iter().eq("loop".encode_utf16()))
        && !context.is_some_and(|value| value.eq_ignore_ascii_case("fork"))
        && background != Some(true)
        && agent.is_none()
        && !allowed_tools.iter().any(|tool| {
            matches!(
                tool.split(['(', ':']).next().unwrap_or(tool).trim(),
                "Agent" | "Task" | "TeamCreate" | "TeamDelete"
            )
        })
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

impl NativeHeadlessCatalogContext {
    fn explain_usage_is_enabled(&self) -> Option<bool> {
        let entrypoint = self.entrypoint.as_ref().and_then(projection_text)?;
        Some(entrypoint == "remote_cowork")
    }

    fn add_dir_is_enabled(&self) -> Option<bool> {
        const REMOTE_ENTRYPOINTS: &[&str] = &[
            "remote",
            "remote_projects",
            "remote_desktop",
            "remote_mobile",
            "ssh-remote",
        ];
        let entrypoint = self.entrypoint.as_ref().and_then(projection_text)?;
        if !REMOTE_ENTRYPOINTS.contains(&entrypoint) {
            return Some(false);
        }
        let (Some(is_child_session), Some(is_claude_code)) =
            (self.is_child_session, self.is_claude_code)
        else {
            return None;
        };
        Some(!is_child_session && !is_claude_code)
    }

    fn auto_mode_setup_is_enabled(&self) -> Option<bool> {
        let (Some(is_remote), Some(model)) = (
            self.is_remote,
            self.selected_model.as_ref().and_then(projection_text),
        ) else {
            return None;
        };
        Some(!is_remote && !model.is_empty())
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
    fn headless_catalog_uses_native_source_order_and_native_gates() {
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

        let first = prompt("first-prompt");
        registry.register_command(first);
        registry.register_command(SlashCommand {
            name: "local-command".into(),
            source: CommandSource::Builtin,
            kind: SlashCommandKind::Builtin {
                handler_id: "local-command".into(),
            },
            // Native `MPe` admits local rows only when supportsNonInteractive is true.
            native_type: Some(NativeCommandType::Local),
            supports_non_interactive: true,
            ..SlashCommand::default()
        });
        let mut manual = prompt("manual-prompt");
        manual.disable_model_invocation = true;
        registry.register_command(manual);
        let mut hidden = prompt("hidden-prompt");
        hidden.is_hidden = true;
        registry.register_command(hidden);
        let mut unavailable = prompt("unavailable-prompt");
        unavailable.is_enabled = false;
        registry.register_command(unavailable);
        // Native `kjs` counts this row in `pe`; `userInvocable` controls the
        // slash UI but is not a headless availability predicate.
        let mut non_user_invocable = prompt("non-user-prompt");
        non_user_invocable.user_invocable = Some(false);
        registry.register_command(non_user_invocable);

        // Replacing metadata does not move or duplicate the original entry.
        let mut replacement = prompt("first-prompt");
        replacement.native_type = Some(NativeCommandType::Prompt);
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
        assert_eq!(catalog.available_command_count, 4);
        assert_eq!(
            catalog
                .prompt_command_names
                .iter()
                .map(|name| name.string_at("").unwrap())
                .collect::<Vec<_>>(),
            ["first-prompt", "non-user-prompt"]
        );
    }

    #[test]
    fn native_builtin_enable_gates_apply_to_bundled_prompts_and_not_custom_skills() {
        let _guard = crate::builtin_support::names::ENV_LOCK.lock().unwrap();
        let mut builtin_doctor = SlashCommand {
            name: "doctor".into(),
            source: CommandSource::Bundled,
            native_source: Some("built-in".into()),
            native_type: Some(NativeCommandType::Prompt),
            aliases: vec!["checkup".into()],
            kind: SlashCommandKind::Bundled {
                frontmatter: Default::default(),
                prompt_fn: None,
            },
            ..SlashCommand::default()
        };
        builtin_doctor.is_enabled = true;
        let mut registry = CommandRegistry::new();
        registry.register_command(builtin_doctor);

        let mut custom_doctor = SlashCommand {
            name: "doctor".into(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            native_type: Some(NativeCommandType::Prompt),
            kind: SlashCommandKind::Markdown {
                file_path: std::path::PathBuf::from("/tmp/doctor.md"),
                frontmatter: Default::default(),
                prompt_template: "custom doctor".into(),
            },
            ..SlashCommand::default()
        };
        custom_doctor.is_enabled = true;
        let mut custom_registry = CommandRegistry::new();
        custom_registry.register_command(custom_doctor);

        std::env::set_var("DISABLE_DOCTOR_COMMAND", "1");
        let builtin_catalog = registry.headless_command_catalog();
        let builtin_palette = registry.palette_commands();
        let custom_catalog = custom_registry.headless_command_catalog();
        std::env::remove_var("DISABLE_DOCTOR_COMMAND");

        assert_eq!(builtin_catalog.available_command_count, 0);
        assert!(builtin_catalog.prompt_command_names.is_empty());
        assert!(builtin_palette.is_empty());
        assert_eq!(custom_catalog.available_command_count, 1);
    }

    #[test]
    fn headless_pool_uses_native_execution_type_and_excludes_unmapped_rows() {
        let mut registry = CommandRegistry::new();
        let mut supported_local = SlashCommand {
            name: "supported-local".into(),
            source: CommandSource::Builtin,
            kind: SlashCommandKind::Builtin {
                handler_id: "supported-local".into(),
            },
            native_type: Some(NativeCommandType::Local),
            supports_non_interactive: true,
            ..SlashCommand::default()
        };
        registry.register_command(supported_local.clone());

        let unsupported_local = SlashCommand {
            name: "unsupported-local".into(),
            source: CommandSource::Builtin,
            kind: SlashCommandKind::Builtin {
                handler_id: "unsupported-local".into(),
            },
            native_type: Some(NativeCommandType::Local),
            ..SlashCommand::default()
        };
        registry.register_command(unsupported_local);

        let interactive = SlashCommand {
            name: "interactive-jsx".into(),
            source: CommandSource::Builtin,
            kind: SlashCommandKind::Builtin {
                handler_id: "interactive-jsx".into(),
            },
            native_type: Some(NativeCommandType::LocalJsx),
            supports_non_interactive: true,
            ..SlashCommand::default()
        };
        registry.register_command(interactive);

        let prompt = SlashCommand {
            name: "headless-prompt".into(),
            source: CommandSource::Builtin,
            kind: SlashCommandKind::Builtin {
                handler_id: "headless-prompt".into(),
            },
            native_type: Some(NativeCommandType::Prompt),
            ..SlashCommand::default()
        };
        registry.register_command(prompt);

        let disabled_prompt = SlashCommand {
            name: "disabled-prompt".into(),
            source: CommandSource::Builtin,
            kind: SlashCommandKind::Builtin {
                handler_id: "disabled-prompt".into(),
            },
            native_type: Some(NativeCommandType::Prompt),
            disable_non_interactive: true,
            ..SlashCommand::default()
        };
        registry.register_command(disabled_prompt);

        // The exact Native MPe predicate has no branch for a missing type, so
        // the row is not admitted to the headless pool.
        registry.register_command(SlashCommand {
            name: "unresolved".into(),
            source: CommandSource::Builtin,
            kind: SlashCommandKind::Builtin {
                handler_id: "unresolved".into(),
            },
            ..SlashCommand::default()
        });
        registry.set_native_headless_context(NativeHeadlessCatalogContext {
            entrypoint: Some("sdk-cli".into()),
            ..NativeHeadlessCatalogContext::default()
        });

        let catalog = registry.headless_command_catalog();
        assert_eq!(catalog.available_command_count, 2);
        assert!(registry.resolve(&"unresolved".into()).is_some());
        assert!(!catalog
            .prompt_command_names
            .iter()
            .any(|name| name.string_at("") == Some("unresolved")));
        assert_eq!(
            catalog
                .prompt_command_names
                .iter()
                .map(|name| name.string_at("").unwrap())
                .collect::<Vec<_>>(),
            ["headless-prompt"]
        );

        supported_local.supports_non_interactive = false;
        assert_eq!(
            supported_local.native_non_interactive_admission(),
            Some(false)
        );
        assert_eq!(
            SlashCommand::default().native_non_interactive_admission(),
            None
        );
    }

    #[test]
    fn builtin_handler_without_native_metadata_preserves_native_hidden_bit() {
        let mut registry = CommandRegistry::new();
        registry.register_command(SlashCommand {
            name: "agents".into(),
            description: "removed Native command".into(),
            source: CommandSource::Builtin,
            kind: SlashCommandKind::Builtin {
                handler_id: "agents".into(),
            },
            is_hidden: true,
            native_type: Some(NativeCommandType::Local),
            supports_non_interactive: true,
            ..SlashCommand::default()
        });

        registry.register_builtin_handler(Arc::new(crate::builtins::AgentsHandler::new(Arc::new(
            orchestrator::test_support::MockOrchestratorHandle::new(),
        ))));

        let agents = registry.resolve(&"agents".into()).unwrap();
        assert!(agents.is_hidden);
        assert_eq!(agents.native_type, Some(NativeCommandType::Local));
        assert_eq!(
            registry.headless_command_catalog().available_command_count,
            0
        );
    }

    #[test]
    fn headless_factory_context_applies_add_dir_auto_mode_and_insights_gates() {
        let mut registry = CommandRegistry::new();
        registry.register_command(SlashCommand {
            name: "add-dir".into(),
            source: CommandSource::Builtin,
            kind: SlashCommandKind::Builtin {
                handler_id: "add-dir".into(),
            },
            native_type: Some(NativeCommandType::Local),
            supports_non_interactive: true,
            ..SlashCommand::default()
        });
        registry.register_command(SlashCommand {
            name: "auto-mode-setup".into(),
            source: CommandSource::Builtin,
            kind: SlashCommandKind::Builtin {
                handler_id: "auto-mode-setup".into(),
            },
            native_type: Some(NativeCommandType::Local),
            supports_non_interactive: true,
            ..SlashCommand::default()
        });
        registry.register_command(SlashCommand {
            name: "insights".into(),
            source: CommandSource::Builtin,
            kind: SlashCommandKind::Builtin {
                handler_id: "insights".into(),
            },
            native_type: Some(NativeCommandType::Prompt),
            requires: Some(crate::model::NativeCommandRequirements {
                workspace: Some(true),
            }),
            policy_gate: Some(crate::model::NativeCommandPolicyGate {
                policy: "allow_insights".into(),
            }),
            ..SlashCommand::default()
        });
        for (name, native_type, supports_non_interactive) in [
            ("extra-usage", NativeCommandType::Local, true),
            ("usage-credits", NativeCommandType::Local, true),
            ("advisor", NativeCommandType::Local, true),
            ("version", NativeCommandType::Local, true),
            ("rate-limit-options", NativeCommandType::LocalJsx, false),
        ] {
            registry.register_command(SlashCommand {
                name: name.into(),
                source: CommandSource::Builtin,
                kind: SlashCommandKind::Builtin {
                    handler_id: name.into(),
                },
                native_type: Some(native_type),
                supports_non_interactive,
                ..SlashCommand::default()
            });
        }
        registry.set_native_headless_context(NativeHeadlessCatalogContext {
            entrypoint: Some("sdk-cli".into()),
            is_child_session: Some(false),
            is_claude_code: Some(false),
            is_remote: Some(false),
            selected_model: Some("claude-sonnet-4-5".into()),
            workspace_available: Some(true),
            insights_policy_gate: Some(false),
            usage_credits_enabled: Some(false),
            advisor_enabled: Some(false),
        });

        let local = registry.headless_command_catalog();
        assert_eq!(local.available_command_count, 1);
        assert!(local.prompt_command_names.is_empty());
        // The Headless fallback names are prompt-only, but the broader model
        // invocable projection also retains enabled local rows whose
        // disableModelInvocation flag is absent.
        assert_eq!(
            registry
                .model_invocable_commands()
                .iter()
                .map(|command| command.name.string_at("").unwrap())
                .collect::<Vec<_>>(),
            ["auto-mode-setup", "rate-limit-options"]
        );

        registry.set_native_headless_context(NativeHeadlessCatalogContext {
            entrypoint: Some("remote".into()),
            is_child_session: Some(false),
            is_claude_code: Some(false),
            is_remote: Some(true),
            selected_model: Some("claude-sonnet-4-5".into()),
            workspace_available: Some(true),
            insights_policy_gate: Some(true),
            usage_credits_enabled: Some(true),
            advisor_enabled: Some(true),
        });
        let remote = registry.headless_command_catalog();
        assert_eq!(remote.available_command_count, 4);
        assert_eq!(
            remote
                .prompt_command_names
                .iter()
                .map(|name| name.string_at("").unwrap())
                .collect::<Vec<_>>(),
            ["insights"]
        );
        assert_eq!(
            registry
                .model_invocable_commands()
                .iter()
                .map(|command| command.name.string_at("").unwrap())
                .collect::<Vec<_>>(),
            [
                "add-dir",
                "insights",
                "usage-credits",
                "advisor",
                "rate-limit-options"
            ]
        );

        registry.set_native_headless_context(NativeHeadlessCatalogContext {
            workspace_available: Some(false),
            insights_policy_gate: Some(true),
            ..NativeHeadlessCatalogContext::default()
        });
        assert!(!registry
            .model_invocable_commands()
            .iter()
            .any(|command| command.name.string_at("") == Some("insights")));
    }

    #[test]
    fn headless_catalog_applies_native_295_bundled_skill_gates() {
        let mut registry = CommandRegistry::new();
        for name in ["cron", "verify", "keybindings-help", "explain-usage"] {
            let mut command = SlashCommand {
                name: name.into(),
                source: CommandSource::Bundled,
                native_source: (name != "cron").then(|| "built-in".into()),
                native_type: Some(NativeCommandType::Prompt),
                kind: SlashCommandKind::Bundled {
                    frontmatter: Default::default(),
                    prompt_fn: None,
                },
                ..SlashCommand::default()
            };
            if name == "verify" {
                // The Native `wvr()` rollout defaults off without a registered
                // reader, so its dynamic disableModelInvocation getter is true.
                command.disable_model_invocation = true;
            }
            if name == "keybindings-help" {
                // Native `Ws()` maps userInvocable:false to isHidden:true.
                command.user_invocable = Some(false);
                command.is_hidden = true;
            }
            registry.register_command(command);
        }

        registry.set_native_headless_context(NativeHeadlessCatalogContext {
            entrypoint: Some("sdk-cli".into()),
            ..NativeHeadlessCatalogContext::default()
        });
        let local = registry.headless_command_catalog();
        assert_eq!(local.available_command_count, 1);
        assert!(local.prompt_command_names.is_empty());

        registry.set_native_headless_context(NativeHeadlessCatalogContext {
            entrypoint: Some("remote_cowork".into()),
            ..NativeHeadlessCatalogContext::default()
        });
        let cowork = registry.headless_command_catalog();
        assert_eq!(cowork.available_command_count, 2);
        assert_eq!(
            cowork
                .prompt_command_names
                .iter()
                .map(|name| name.string_at("").unwrap())
                .collect::<Vec<_>>(),
            ["explain-usage"]
        );
    }

    #[test]
    fn handler_replacement_keeps_existing_prompt_model_invocation_gate() {
        let mut registry = CommandRegistry::new();
        registry.register_command(SlashCommand {
            name: "cron".into(),
            description: "Explicit local schedule management".into(),
            source: CommandSource::Bundled,
            native_type: Some(NativeCommandType::Prompt),
            kind: SlashCommandKind::Bundled {
                frontmatter: Default::default(),
                prompt_fn: None,
            },
            disable_model_invocation: true,
            ..SlashCommand::default()
        });

        registry.register_builtin_handler(Arc::new(UnimplementedCommandHandler::new(
            "cron",
            "Explicit local schedule management",
        )));

        let cron = registry.resolve(&"cron".into()).unwrap();
        assert!(cron.disable_model_invocation);
        assert!(cron.native_source.is_none());
        assert!(registry
            .headless_command_catalog()
            .prompt_command_names
            .is_empty());
    }

    #[test]
    fn custom_project_cron_remains_in_native_headless_catalog() {
        let mut registry = CommandRegistry::new();
        registry.register_command(SlashCommand {
            name: "cron".into(),
            source: CommandSource::Settings(lingxi_core::types::SettingsScope::Project),
            kind: SlashCommandKind::Markdown {
                file_path: std::path::PathBuf::from(branding::DOT_DIR).join("commands/cron.md"),
                frontmatter: Default::default(),
                prompt_template: "Project cron instructions".into(),
            },
            ..SlashCommand::default()
        });

        let catalog = registry.headless_command_catalog();
        assert_eq!(catalog.available_command_count, 1);
        assert_eq!(
            catalog
                .prompt_command_names
                .iter()
                .map(|name| name.string_at("").unwrap())
                .collect::<Vec<_>>(),
            ["cron"]
        );
    }

    #[test]
    fn native_source_projection_places_user_skills_before_plugin_and_bundled_rows() {
        use lingxi_core::types::SettingsScope;
        use std::path::PathBuf;

        let mut registry = CommandRegistry::new();
        let register =
            |registry: &mut CommandRegistry, name: &str, source: CommandSource, is_prompt: bool| {
                registry.register_command(SlashCommand {
                    name: name.into(),
                    source,
                    native_type: is_prompt.then_some(NativeCommandType::Prompt),
                    ..SlashCommand::default()
                });
            };
        register(&mut registry, "builtin", CommandSource::Builtin, false);
        register(
            &mut registry,
            "plugin-authoring",
            CommandSource::Bundled,
            true,
        );
        register(
            &mut registry,
            "project-skill",
            CommandSource::Settings(SettingsScope::Project),
            false,
        );
        register(
            &mut registry,
            "local-skill",
            CommandSource::Settings(SettingsScope::Local),
            false,
        );
        register(
            &mut registry,
            "plugin-command",
            CommandSource::Plugin,
            false,
        );
        register(
            &mut registry,
            "graphify",
            CommandSource::Settings(SettingsScope::User),
            true,
        );
        register(
            &mut registry,
            "managed-skill",
            CommandSource::Settings(SettingsScope::Managed),
            false,
        );
        register(&mut registry, "mcp-prompt", CommandSource::Mcp, false);
        let plugin_skill = SlashCommand {
            name: "plugin-skill".into(),
            source: CommandSource::Plugin,
            native_type: Some(NativeCommandType::Prompt),
            skill_root: Some(PathBuf::from("/plugins/example/skills/plugin-skill")),
            ..SlashCommand::default()
        };
        registry.register_command(plugin_skill);
        register(
            &mut registry,
            "second-bundled",
            CommandSource::Bundled,
            false,
        );

        let source_order = registry
            .native_source_ordered_commands()
            .into_iter()
            .map(|command| command.name.string_at("").unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            source_order,
            [
                "managed-skill",
                "graphify",
                "project-skill",
                "local-skill",
                "plugin-command",
                "plugin-skill",
                "plugin-authoring",
                "second-bundled",
                "builtin",
                "mcp-prompt",
            ]
        );

        let catalog = registry.headless_command_catalog();
        let prompt_names = catalog
            .prompt_command_names
            .iter()
            .map(|name| name.string_at("").unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            prompt_names,
            ["graphify", "plugin-skill", "plugin-authoring"]
        );
    }

    #[test]
    fn native_source_projection_groups_plugin_commands_before_skills_for_alias_resolution() {
        use std::path::PathBuf;

        fn plugin_command(
            plugin_name: &str,
            local_name: &str,
            plugin_id: PluginId,
            display_name: Option<&str>,
        ) -> SlashCommand {
            let base_dir = PathBuf::from(format!("/plugins/{plugin_name}/commands"));
            let file_path = base_dir.join(format!("{local_name}.md"));
            let raw = display_name.map_or_else(
                || "---\ndescription: plugin command\n---\n\nCommand body\n".to_owned(),
                |name| {
                    format!("---\nname: {name}\ndescription: plugin command\n---\n\nCommand body\n")
                },
            );
            let file = crate::parse_command_markdown(
                &raw,
                file_path.clone(),
                base_dir,
                CommandSource::Plugin,
            );
            let base = crate::build_markdown_command(&file, CommandSource::Plugin);
            let canonical_name: Utf16JsonProjection = format!("{plugin_name}:{local_name}").into();
            let (user_facing_name, aliases) =
                crate::plugin_prompt_name_metadata(&canonical_name, file.frontmatter_name.as_ref());
            let (frontmatter, prompt_template) = match base.kind {
                SlashCommandKind::Markdown {
                    frontmatter,
                    prompt_template,
                    ..
                } => (frontmatter, prompt_template),
                _ => unreachable!("markdown factory returns a markdown command"),
            };
            SlashCommand {
                name: canonical_name,
                source: CommandSource::Plugin,
                kind: SlashCommandKind::Plugin {
                    plugin_id,
                    file_path,
                    frontmatter,
                    prompt_template,
                },
                loaded_from: Some("plugin".to_owned()),
                plugin_name: Some(plugin_name.into()),
                native_source: Some("plugin".into()),
                user_facing_name: Some(user_facing_name),
                aliases,
                ..base
            }
        }

        fn plugin_skill(
            plugin_name: &str,
            local_name: &str,
            plugin_id: PluginId,
            display_name: Option<&str>,
        ) -> SlashCommand {
            let skill_root = PathBuf::from(format!("/plugins/{plugin_name}/skills/{local_name}"));
            let file_path = skill_root.join("SKILL.md");
            let raw = display_name.map_or_else(
                || "---\ndescription: plugin skill\n---\n\nSkill body\n".to_owned(),
                |name| format!("---\nname: {name}\ndescription: plugin skill\n---\n\nSkill body\n"),
            );
            let file = crate::parse_skill_command_markdown(
                &raw,
                file_path.clone(),
                skill_root,
                CommandSource::Plugin,
            );
            let base = crate::build_skill_command(&file, CommandSource::Plugin);
            let canonical_name: Utf16JsonProjection = format!("{plugin_name}:{local_name}").into();
            let (user_facing_name, aliases) =
                crate::plugin_prompt_name_metadata(&canonical_name, file.frontmatter_name.as_ref());
            let (frontmatter, prompt_template) = match base.kind {
                SlashCommandKind::Markdown {
                    frontmatter,
                    prompt_template,
                    ..
                } => (frontmatter, prompt_template),
                _ => unreachable!("skill factory returns a markdown command"),
            };
            SlashCommand {
                name: canonical_name,
                source: CommandSource::Plugin,
                kind: SlashCommandKind::Plugin {
                    plugin_id,
                    file_path,
                    frontmatter,
                    prompt_template,
                },
                loaded_from: Some("plugin".to_owned()),
                plugin_name: Some(plugin_name.into()),
                native_source: Some("plugin".into()),
                user_facing_name: Some(user_facing_name),
                aliases,
                ..base
            }
        }

        let plugin_one = PluginId::new();
        let plugin_two = PluginId::new();
        let mut registry = CommandRegistry::new();

        // PluginManager installs each plugin as one vector. Across plugins the
        // physical sequence is command-1, skill-1, command-2, skill-2.
        registry.register_plugin_commands(
            plugin_one,
            vec![
                plugin_command("plugin-one", "command-one", plugin_one, None),
                plugin_skill("plugin-one", "skill-one", plugin_one, Some("review")),
            ],
        );
        registry.register_plugin_commands(
            plugin_two,
            vec![
                plugin_command("plugin-two", "command-two", plugin_two, Some("review")),
                plugin_skill("plugin-two", "skill-two", plugin_two, None),
            ],
        );

        let registered_order = registry
            .registration_ordered_commands()
            .iter()
            .map(|command| command.name.string_at("").unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            registered_order,
            [
                "plugin-one:command-one",
                "plugin-one:skill-one",
                "plugin-two:command-two",
                "plugin-two:skill-two",
            ]
        );

        let native_order = registry.native_source_ordered_commands();
        assert_eq!(
            native_order
                .iter()
                .map(|command| command.name.string_at("").unwrap())
                .collect::<Vec<_>>(),
            [
                "plugin-one:command-one",
                "plugin-two:command-two",
                "plugin-one:skill-one",
                "plugin-two:skill-two",
            ]
        );
        assert!(native_order[0].skill_root.is_none());
        assert!(native_order[1].skill_root.is_none());
        assert!(native_order[2].skill_root.is_some());
        assert!(native_order[3].skill_root.is_some());

        // Native Hs checks canonical names first, then returns the first
        // display-name/name/alias match. No canonical name or display name is
        // "review" here, so the earlier plugin command must own the alias
        // before the earlier-registered plugin skill can match it.
        let first_review_match = native_order
            .iter()
            .find(|command| {
                command.name.string_at("") == Some("review")
                    || command
                        .user_facing_name
                        .as_ref()
                        .and_then(|name| name.string_at(""))
                        == Some("review")
                    || command
                        .aliases
                        .iter()
                        .any(|alias| alias.string_at("") == Some("review"))
            })
            .expect("the plugin command and skill share the Native review alias");
        assert_eq!(
            first_review_match.name.string_at(""),
            Some("plugin-two:command-two")
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

    #[cfg(feature = "builtin-handlers")]
    #[test]
    fn builtin_prompt_handler_projects_native_context_source() {
        let mut reg = CommandRegistry::new();
        reg.register_builtin_handler(Arc::new(crate::builtins::InitHandler::new()));
        reg.register_builtin_handler(Arc::new(UnimplementedCommandHandler::new("login", "Login")));

        assert_eq!(
            reg.resolve(&"init".into())
                .and_then(|command| command.native_source.as_ref())
                .and_then(|source| source.string_at("")),
            Some("built-in")
        );
        assert!(reg
            .resolve(&"login".into())
            .is_some_and(|command| command.native_source.is_none()));
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
    #[test]
    fn side_chat_palette_keeps_opaque_inline_skill_names() {
        fn inline_skill(name: &str) -> SlashCommand {
            let mut command = markdown_cmd(name, vec![]);
            command.kind = SlashCommandKind::Markdown {
                file_path: std::path::PathBuf::from(format!("{name}.md")),
                frontmatter: Default::default(),
                prompt_template: "".into(),
            };
            command
        }

        let mut registry = CommandRegistry::new();
        registry.set_side_chat(true);
        let units = vec![b'x' as u16, 0xd800];
        let mut inline = inline_skill("display");
        inline.name = projection_from_units(units.clone());
        registry.register_command(inline);
        registry.register_command(inline_skill("loop"));
        let commands = registry.palette_commands();
        assert!(commands.iter().any(|command| {
            command.source == CommandSource::Settings(lingxi_core::types::SettingsScope::Project)
                && matches!(&command.kind, SlashCommandKind::Markdown { .. })
                && command.name.string_units("") == Some(units.clone())
        }));
        assert!(!commands
            .iter()
            .any(|command| command.name.string_at("") == Some("loop")));
    }
}
