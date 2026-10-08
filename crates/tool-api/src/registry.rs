//! `ToolRegistry` — partitioned, dynamic registry of `Tool` implementations.
//!
//! The registry holds builtin tools (statically registered at startup) plus
//! four dynamic partitions:
//!
//! - **MCP**: tools sourced from MCP server connections, keyed by
//!   [`McpConnectionId`] so a disconnect can drop them en masse.
//! - **LSP**: tools sourced from Language-Server-Protocol bridges.
//! - **Plugin**: tools sourced from host plugins, keyed by [`PluginId`]
//!   so an unloaded plugin can drop its tools.
//! - **Mod**: runtime-registered tools owned by a session plugin, kept apart
//!   from MCP catalog snapshots so reconnects cannot erase them.
//!
//! Each lookup checks all partitions. The wire / prompt order is parity-fixed
//! by [`available_tools`](ToolRegistry::available_tools): builtins sorted by
//! `locale_cmp` as a contiguous prefix, then the MCP / LSP / plugin partitions
//! sorted by `locale_cmp` — mirroring claude-code's `assembleToolPool` /
//! `mergeAndFilterTools` (`tools.ts:345-367`, `utils/toolPool.ts:65-70`). The
//! MCP / plugin partitions are stored insertion-ordered (`Vec` of keyed
//! entries, not `HashMap`) so that order is deterministic before the sort.

use crate::defer::DeferralState;
use crate::tool_search_view::{SharedToolSearchView, ToolSearchEntry};
use crate::tool_trait::{BashPrecommitSkills, Tool, ToolStaticContext};
use crate::wire::locale_cmp;
use lingxi_core::types::{McpConnectionId, PluginId};
use std::sync::{Arc, RwLock};

/// Built-in tools that can execute commands/code, plus WebFetch's explicitly
/// restricted network surface. Dynamic MCP/LSP/plugin tools are intentionally
/// left alone; they have their own policy/provenance controls.
pub const RESTRICTED_DEFAULT_BUILTIN_DENY: &[&str] = &[
    "Agent",
    "Bash",
    "CronCreate",
    "PowerShell",
    "REPL",
    "RemoteTrigger",
    "WebFetch",
    "Workflow",
];

/// Registry of all [`Tool`] instances available to the dispatcher.
///
/// Cloning is not supported; share via `Arc<ToolRegistry>` instead.
///
/// The MCP, Mod, and plugin partitions are insertion-ordered `Vec`s of `(key, tools)`
/// entries (rather than a `HashMap`) so iteration order is deterministic; the
/// dedup/sort that produces the wire order is applied in [`Self::available_tools`].
pub struct ToolRegistry {
    builtin: Vec<Arc<dyn Tool>>,
    // MCP servers may send `notifications/tools/list_changed` after startup.
    // Keep only this dynamic partition behind an interior lock so the shared
    // `Arc<ToolRegistry>` can replace one server's tools without rebuilding the
    // immutable builtin/LSP/plugin partitions.
    mcp_tools: RwLock<Vec<(McpConnectionId, Vec<Arc<dyn Tool>>)>>,
    // Runtime Mod tools are session-owned. MCP catalog rebuilds must not erase
    // them while a registered Mod is still loaded.
    mod_tools: RwLock<Vec<(String, Vec<Arc<dyn Tool>>)>>,
    lsp_tools: Vec<Arc<dyn Tool>>,
    plugin_tools: Vec<(PluginId, Vec<Arc<dyn Tool>>)>,
    /// Shared Tool Search deferral state (mode + loaded-set). Disabled by
    /// default; wired to the env-derived mode by `tool_meta::register_all`.
    /// Shared with the `ToolSearchTool` and read by the orchestrator's wire
    /// assembly ([`crate::wire::apply_defer_loading`]) so both ends agree.
    deferral: Arc<DeferralState>,
    /// Live searchable view of the DEFERRED tool set, fed to `ToolSearchTool` at
    /// registration and refreshed by [`Self::refresh_tool_search_view`] once the
    /// registry (including MCP tools) is fully assembled.
    tool_search_view: Arc<SharedToolSearchView>,
    /// Optional session-scoped built-in filter. `None` preserves historical
    /// behavior; restricted sessions install this before sharing the registry.
    builtin_filter: Option<BuiltinToolFilter>,
    /// Tools required by explicit execution configuration, such as the
    /// schema capture tool. These survive `--tools` selection but still obey
    /// the complete session capability allowlist.
    required_builtins: std::collections::HashSet<String>,
    /// Composition-level built-in catalog, independent of user selection and
    /// dynamic MCP tools. Required execution tools are added after this catalog.
    builtin_catalog: Option<std::collections::HashSet<String>>,
    /// Optional complete session capability allowlist. Unlike
    /// `builtin_filter`, this applies to every registry partition so a dynamic
    /// MCP/LSP/plugin refresh cannot re-introduce a tool hidden by the session
    /// profile.
    session_allowlist: Option<std::collections::HashSet<String>>,
    /// The session's canonical main-loop model id (claude-code `J$e()`).
    /// Behind a lock because `/model` moves it mid-session while the registry
    /// is already shared as an `Arc`, and because the Tool Search refreshers
    /// take `&self`.
    main_loop_model: RwLock<Option<String>>,
    /// The owning session's live skill catalog projection, also used by child tools.
    bash_precommit_skills_provider: RwLock<Option<BashPrecommitSkillsProvider>>,
    bash_precommit_session_generation: std::sync::atomic::AtomicU64,
}

type BashPrecommitSkillsProvider = Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = BashPrecommitSkills> + Send>>
        + Send
        + Sync,
>;

#[derive(Clone, Debug)]
struct BuiltinToolFilter {
    /// `Some` is the complete explicit `--tools` allowlist. `None` uses the
    /// restricted default deny set above.
    explicit_allowlist: Option<std::collections::HashSet<String>>,
}

impl BuiltinToolFilter {
    fn allows(&self, name: &str) -> bool {
        match &self.explicit_allowlist {
            Some(allowed) => allowed.contains(name),
            None => !RESTRICTED_DEFAULT_BUILTIN_DENY.contains(&name),
        }
    }
}

impl ToolRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            builtin: Vec::new(),
            mcp_tools: RwLock::new(Vec::new()),
            mod_tools: RwLock::new(Vec::new()),
            lsp_tools: Vec::new(),
            plugin_tools: Vec::new(),
            deferral: Arc::new(DeferralState::disabled()),
            tool_search_view: Arc::new(SharedToolSearchView::new()),
            builtin_filter: None,
            required_builtins: std::collections::HashSet::new(),
            builtin_catalog: None,
            session_allowlist: None,
            main_loop_model: RwLock::new(None),
            bash_precommit_skills_provider: RwLock::new(None),
            bash_precommit_session_generation: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Share a live skill projection with subagents using this registry.
    pub fn set_bash_precommit_skills_provider<F, Fut>(&self, provider: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = BashPrecommitSkills> + Send + 'static,
    {
        *self
            .bash_precommit_skills_provider
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(Arc::new(move || Box::pin(provider())));
    }

    /// Read the session's skill suggestions without holding a registry lock across await.
    pub async fn bash_precommit_skills(&self) -> BashPrecommitSkills {
        let provider = self
            .bash_precommit_skills_provider
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match provider {
            Some(provider) => provider().await,
            None => BashPrecommitSkills::default(),
        }
    }

    /// Identify the current conversation activation for memoized Bash guidance.
    pub fn bash_precommit_session_generation(&self) -> u64 {
        self.bash_precommit_session_generation
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Start a fresh guidance latch when the registry is reused for another session.
    pub fn reset_bash_precommit_prompt_session(&self) {
        self.bash_precommit_session_generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Install the restricted-session built-in filter. Values are flattened by
    /// comma/whitespace and legacy aliases are normalized by the permission
    /// crate, keeping `--tools` compatible with its existing parser surface.
    /// `None` installs the restricted default deny set; an explicit empty
    /// selection disables ordinary built-ins, including in unrestricted sessions.
    pub fn set_restricted_builtin_filter(&mut self, tools: Option<&[String]>) {
        if tools.is_some_and(|values| {
            values
                .iter()
                .any(|value| value.split(',').any(|name| name.trim() == "default"))
        }) {
            self.builtin_filter = None;
            return;
        }
        let explicit_allowlist = tools.map(|values| {
            values
                .iter()
                // Clap already tokenizes space-separated `--tools` values;
                // keep whitespace inside `Bash(git *)` intact and only split
                // comma-separated entries here.
                .flat_map(|value| value.split(','))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| permission::PermissionRuleValue::from_rule_string(value).tool_name)
                .collect()
        });
        self.builtin_filter = Some(BuiltinToolFilter { explicit_allowlist });
    }

    /// Select the built-in catalog for a composition without hiding explicitly
    /// configured dynamic MCP tools or changing empty `--tools` Mod policy.
    pub fn set_builtin_catalog(&mut self, names: &[&str]) {
        self.builtin_catalog = Some(names.iter().map(|name| (*name).to_owned()).collect());
    }

    /// Install a complete session capability allowlist for built-ins and hide
    /// every dynamic MCP/LSP/plugin partition. Tool aliases never bypass this
    /// gate: the canonical built-in name must be present in the allowlist.
    pub fn set_session_tool_allowlist(&mut self, tools: &[String]) {
        self.session_allowlist = Some(
            tools
                .iter()
                .map(|value| permission::PermissionRuleValue::from_rule_string(value).tool_name)
                .collect(),
        );
        // The ToolSearch tool holds a shared materialized view. Rebuild it at
        // the same boundary so a policy installed after tool registration
        // cannot leave previously-visible denied tools searchable until the
        // next request assembly.
        self.refresh_tool_search_view();
    }

    fn session_allows(&self, name: &str) -> bool {
        self.session_allowlist
            .as_ref()
            .is_none_or(|allowed| allowed.contains(name))
    }

    fn session_allows_dynamic(&self, name: &str) -> bool {
        self.session_allowlist.is_none() && self.session_allows(name)
    }

    /// Whether an explicit empty tool selection disables Mod registration.
    #[must_use]
    pub fn mod_registration_disabled(&self) -> bool {
        self.builtin_filter
            .as_ref()
            .and_then(|filter| filter.explicit_allowlist.as_ref())
            .is_some_and(std::collections::HashSet::is_empty)
            || self
                .session_allowlist
                .as_ref()
                .is_some_and(std::collections::HashSet::is_empty)
    }

    fn builtin_allowed(&self, name: &str) -> bool {
        self.session_allows(name)
            && (self.required_builtins.contains(name)
                || (self
                    .builtin_catalog
                    .as_ref()
                    .is_none_or(|catalog| catalog.contains(name))
                    && self
                        .builtin_filter
                        .as_ref()
                        .is_none_or(|filter| filter.allows(name))))
    }

    /// The shared Tool Search deferral state.
    #[must_use]
    pub fn deferral(&self) -> &Arc<DeferralState> {
        &self.deferral
    }

    /// Install the session's deferral state (called by `tool_meta::register_all`
    /// with the env-derived mode). The same `Arc` is handed to the
    /// `ToolSearchTool`, so both the wire serializer and the search consumer
    /// observe one shared loaded-set.
    pub fn set_deferral(&mut self, deferral: Arc<DeferralState>) {
        self.deferral = deferral;
    }

    /// The shared live view cell fed to the `ToolSearchTool`.
    #[must_use]
    pub fn tool_search_view(&self) -> Arc<SharedToolSearchView> {
        self.tool_search_view.clone()
    }

    /// Recompute the searchable view from the registry's deferred-candidate set and
    /// publish it to the shared cell. Call once after the registry (including MCP
    /// tools) is fully assembled. When the deferral state is disabled the
    /// deferred set is empty, so the view is emptied — the correct behavior for a
    /// non-tool-search session.
    ///
    /// The entry `description` is left empty here because the tool prompt is
    /// async. Request assembly calls [`Self::refresh_tool_search_view_from_wire`]
    /// after serialization to publish the actual descriptions.
    pub fn refresh_tool_search_view(&self) {
        let ctx = ToolStaticContext::default();
        let entries: Vec<ToolSearchEntry> = self
            .available_tools(&ctx)
            .iter()
            .filter(|t| {
                self.deferral.wants_defer(t.name(), t.should_defer()) && self.deferral.is_enabled()
            })
            .map(|t| ToolSearchEntry {
                name: t.name().to_string(),
                description: String::new(),
                search_hint: t.search_hint().map(str::to_string),
            })
            .collect();
        self.tool_search_view.set_entries(entries);
    }

    /// Refresh the deferred search view and fill descriptions from the complete
    /// pre-filter wire schema list. This keeps ToolSearch keyword scoring on the
    /// same long-form descriptions the model would receive after discovery.
    pub fn refresh_tool_search_view_from_wire(
        &self,
        wire: &[lingxi_core::types::utf16_json::Utf16JsonProjection],
    ) {
        let descriptions: std::collections::HashMap<&str, &str> = wire
            .iter()
            .filter_map(|entry| {
                Some((
                    entry.get("name")?.as_str()?,
                    entry.get("description")?.as_str()?,
                ))
            })
            .collect();
        let ctx = ToolStaticContext::default();
        let entries = self
            .available_tools(&ctx)
            .iter()
            .filter(|tool| {
                self.deferral.is_enabled()
                    && self.deferral.wants_defer(tool.name(), tool.should_defer())
            })
            .map(|tool| ToolSearchEntry {
                name: tool.name().to_string(),
                description: descriptions
                    .get(tool.name())
                    .copied()
                    .unwrap_or_default()
                    .to_string(),
                search_hint: tool.search_hint().map(str::to_string),
            })
            .collect();
        self.tool_search_view.set_entries(entries);
    }

    /// Register a builtin tool. Insertion order is preserved.
    pub fn register_builtin(&mut self, tool: Arc<dyn Tool>) {
        self.builtin.push(tool);
    }

    /// Register a tool required to fulfill explicit execution configuration.
    /// This bypasses only the built-in selection filter, never the complete
    /// session capability allowlist or the tool's own enablement gate.
    pub fn register_required_builtin(&mut self, tool: Arc<dyn Tool>) {
        self.required_builtins.insert(tool.name().to_owned());
        self.register_builtin(tool);
    }

    /// Publish the session's main-loop model, the port of claude-code's
    /// `Dx(()=>HR(rt()))` registration of `mainLoopCanonical`.
    ///
    /// The value must already be canonical — alias-resolved and `[1m]`-stripped,
    /// i.e. what `HR()` returns — because [`crate::todo_tools_gate`] matches it
    /// against the oracle's literal model regex. The orchestrator republishes it
    /// each turn from the live session, so `/model` switches and resumes are
    /// picked up without a second wiring point.
    ///
    /// Takes `&self`: the registry is shared as an `Arc` by the time a session
    /// is running.
    pub fn set_main_loop_model(&self, model: Option<String>) {
        *self
            .main_loop_model
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = model;
    }

    /// The session's main-loop model, if one has been published.
    #[must_use]
    pub fn main_loop_model(&self) -> Option<String> {
        self.main_loop_model
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Fill [`ToolStaticContext::main_loop_model`] from the registry when the
    /// caller left it empty, so every advertise path evaluates the same gate
    /// without each call site having to reach for the session. An explicit
    /// value on the incoming context wins, which is what lets a test pin a
    /// model without touching registry state.
    fn effective_static_ctx(&self, ctx: &ToolStaticContext) -> ToolStaticContext {
        if ctx.main_loop_model.is_some() {
            return ctx.clone();
        }
        ToolStaticContext {
            main_loop_model: self.main_loop_model(),
            ..ctx.clone()
        }
    }

    /// Return all tools enabled for the given static context, in claude-code's
    /// wire order: builtins (gated on [`Tool::is_enabled`]) sorted by
    /// [`locale_cmp`] as a contiguous prefix, then the MCP + LSP + plugin tools
    /// sorted by [`locale_cmp`]. This mirrors `assembleToolPool` /
    /// `mergeAndFilterTools` (`tools.ts:345-367`, `utils/toolPool.ts:65-70`),
    /// which partition-sort with `name.localeCompare` keeping built-ins a
    /// contiguous prefix for the server-side prompt-cache breakpoint. Builtins
    /// win on a name conflict (their entry is emitted; the later dynamic one is
    /// dropped), matching `uniqBy`'s built-in-precedence dedup.
    ///
    /// MCP / LSP / plugin tools follow the optional session capability
    /// allowlist in addition to their owning subsystem's lifecycle policy.
    #[must_use]
    pub fn available_tools(&self, ctx: &ToolStaticContext) -> Vec<Arc<dyn Tool>> {
        let ctx = &self.effective_static_ctx(ctx);
        // Builtin prefix: enabled builtins, locale-sorted by name.
        let mut builtins: Vec<Arc<dyn Tool>> = self
            .builtin
            .iter()
            .filter(|t| self.builtin_allowed(t.name()) && t.is_enabled(ctx))
            .cloned()
            .collect();
        builtins.sort_by(|a, b| locale_cmp(a.name(), b.name()));

        // Dynamic partition: MCP + Mod + LSP + plugin, locale-sorted by name.
        let mut dynamic: Vec<Arc<dyn Tool>> = Vec::new();
        let mcp_tools = self
            .mcp_tools
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_id, ts) in mcp_tools.iter() {
            dynamic.extend(
                ts.iter()
                    .filter(|tool| self.session_allows_dynamic(tool.name()))
                    .cloned(),
            );
        }
        for (_plugin, ts) in self
            .mod_tools
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
        {
            dynamic.extend(
                ts.iter()
                    .filter(|tool| self.session_allows_dynamic(tool.name()))
                    .cloned(),
            );
        }
        dynamic.extend(
            self.lsp_tools
                .iter()
                .filter(|tool| self.session_allows_dynamic(tool.name()))
                .cloned(),
        );
        for (_id, ts) in &self.plugin_tools {
            dynamic.extend(
                ts.iter()
                    .filter(|tool| self.session_allows_dynamic(tool.name()))
                    .cloned(),
            );
        }
        dynamic.sort_by(|a, b| locale_cmp(a.name(), b.name()));

        // Builtin-precedence dedup (uniqBy): drop a dynamic tool whose name
        // collides with a builtin already in the prefix.
        let builtin_names: std::collections::HashSet<String> =
            builtins.iter().map(|t| t.name().to_string()).collect();
        let mut out = builtins;
        for t in dynamic {
            if !builtin_names.contains(t.name()) {
                out.push(t);
            }
        }
        out
    }

    /// Find a tool by name or alias across all partitions, ignoring the
    /// session allowlist and builtin filter. This is the full registered
    /// catalog (`Pk()`), used when an unknown-tool suffix needs to know
    /// whether a name is a real product tool that is merely hidden here.
    #[must_use]
    pub fn find_registered(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if let Some(tool) = self
            .builtin
            .iter()
            .find(|t| t.name() == name || t.aliases().contains(&name))
        {
            return Some(tool.clone());
        }
        {
            let mcp_tools = self
                .mcp_tools
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(tool) = mcp_tools
                .iter()
                .flat_map(|(_id, tools)| tools.iter())
                .find(|t| t.name() == name || t.aliases().contains(&name))
            {
                return Some(tool.clone());
            }
        }
        if let Some(tool) = self
            .mod_tools
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .flat_map(|(_, tools)| tools.iter())
            .find(|tool| tool.name() == name || tool.aliases().contains(&name))
        {
            return Some(tool.clone());
        }
        self.lsp_tools
            .iter()
            .chain(self.plugin_tools.iter().flat_map(|(_id, ts)| ts.iter()))
            .find(|t| t.name() == name || t.aliases().contains(&name))
            .cloned()
    }

    /// Find a tool by name or alias across all partitions. Returns the first
    /// matching tool in iteration order (builtin first).
    #[must_use]
    pub fn find_by_name(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if let Some(tool) = self.builtin.iter().find(|t| {
            self.builtin_allowed(t.name()) && (t.name() == name || t.aliases().contains(&name))
        }) {
            return Some(tool.clone());
        }
        {
            let mcp_tools = self
                .mcp_tools
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(tool) = mcp_tools
                .iter()
                .flat_map(|(_id, tools)| tools.iter())
                .find(|t| {
                    self.session_allows_dynamic(t.name())
                        && (t.name() == name || t.aliases().contains(&name))
                })
            {
                return Some(tool.clone());
            }
        }
        if let Some(tool) = self
            .mod_tools
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .flat_map(|(_, tools)| tools.iter())
            .find(|tool| {
                self.session_allows_dynamic(tool.name())
                    && (tool.name() == name || tool.aliases().contains(&name))
            })
        {
            return Some(tool.clone());
        }
        self.lsp_tools
            .iter()
            .chain(self.plugin_tools.iter().flat_map(|(_id, ts)| ts.iter()))
            .find(|t| {
                self.session_allows_dynamic(t.name())
                    && (t.name() == name || t.aliases().contains(&name))
            })
            .cloned()
    }

    /// Register all tools exposed by a freshly-connected MCP server. If the
    /// connection id was already registered, its tool set is replaced in place
    /// (preserving its insertion position), matching the prior `HashMap::insert`
    /// upsert semantics.
    pub fn register_mcp_tools(&self, conn_id: McpConnectionId, tools: Vec<Arc<dyn Tool>>) {
        let mut mcp_tools = self
            .mcp_tools
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = mcp_tools.iter_mut().find(|(id, _)| *id == conn_id) {
            entry.1 = tools;
        } else {
            mcp_tools.push((conn_id, tools));
        }
    }

    /// Drop every tool sourced from the given MCP connection.
    pub fn unregister_mcp_tools(&self, conn_id: McpConnectionId) {
        self.mcp_tools
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(id, _)| *id != conn_id);
    }

    /// Atomically replace the entire MCP partition set from a rebuilt shared
    /// session snapshot, then refresh the deferred-search view against that
    /// new partition set.
    pub fn replace_mcp_tools(&self, tools: Vec<(McpConnectionId, Vec<Arc<dyn Tool>>)>) {
        *self
            .mcp_tools
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = tools;
        self.refresh_tool_search_view();
    }

    /// Insert or replace one live tool owned by a Mod.
    pub fn register_mod_tool(&self, plugin: &str, tool: Arc<dyn Tool>) {
        let mut mods = self
            .mod_tools
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tools = if let Some((_, tools)) = mods.iter_mut().find(|(name, _)| name == plugin) {
            tools
        } else {
            mods.push((plugin.to_owned(), Vec::new()));
            &mut mods.last_mut().expect("just inserted").1
        };
        if let Some(existing) = tools
            .iter_mut()
            .find(|existing| existing.name() == tool.name())
        {
            *existing = tool;
        } else {
            tools.push(tool);
        }
        drop(mods);
        self.refresh_tool_search_view();
    }

    /// Check whether a full tool name is already owned by this Mod.
    #[must_use]
    pub fn has_mod_tool(&self, plugin: &str, name: &str) -> bool {
        self.mod_tools
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|(owner, tools)| owner == plugin && tools.iter().any(|tool| tool.name() == name))
    }

    /// The owning Mod of a runtime-registered tool, for `tool.describe`'s
    /// provider identity. MCP catalog refreshes cannot change this partition.
    pub fn mod_tool_owner(&self, name: &str) -> Option<String> {
        self.mod_tools
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(_, tools)| tools.iter().any(|tool| tool.name() == name))
            .map(|(owner, _)| owner.clone())
    }

    /// Remove every live tool owned by a Mod on unload.
    pub fn unregister_mod_tools(&self, plugin: &str) {
        self.mod_tools
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(name, _)| name != plugin);
        self.refresh_tool_search_view();
    }

    /// Register all tools exposed by a freshly-loaded plugin. Re-registering an
    /// existing plugin id replaces its tool set in place (upsert).
    pub fn register_plugin_tools(&mut self, plugin_id: PluginId, tools: Vec<Arc<dyn Tool>>) {
        if let Some(entry) = self
            .plugin_tools
            .iter_mut()
            .find(|(id, _)| *id == plugin_id)
        {
            entry.1 = tools;
        } else {
            self.plugin_tools.push((plugin_id, tools));
        }
    }

    /// Drop every tool sourced from the given plugin.
    pub fn unregister_plugin(&mut self, plugin_id: &PluginId) {
        self.plugin_tools.retain(|(id, _)| id != plugin_id);
    }

    /// Return ALL registered tool names across every partition (builtin
    /// + MCP + LSP + plugin), unfiltered by `Tool::is_enabled`.
    ///
    /// Used by `lingxi-orchestrator`'s M5-03 system prompt assembler:
    /// the `<tools>` block emits names alphabetically so the model
    /// knows what to call by name. Filtering by enable-flag would
    /// require a `ToolStaticContext`, which only carries meaning at
    /// dispatch time. The order returned here is partition iteration
    /// order; the assembler re-sorts alphabetically for byte stability.
    #[must_use]
    pub fn all_names(&self) -> Vec<String> {
        let mut names = self
            .builtin
            .iter()
            .filter(|t| self.builtin_allowed(t.name()))
            .map(|t| t.name().to_string())
            .collect::<Vec<_>>();
        names.extend(
            self.mcp_tools
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .flat_map(|(_id, tools)| tools.iter())
                .filter(|tool| self.session_allows_dynamic(tool.name()))
                .map(|t| t.name().to_string()),
        );
        names.extend(
            self.mod_tools
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .flat_map(|(_, tools)| tools.iter())
                .filter(|tool| self.session_allows_dynamic(tool.name()))
                .map(|tool| tool.name().to_string()),
        );
        names.extend(
            self.lsp_tools
                .iter()
                .filter(|tool| self.session_allows_dynamic(tool.name()))
                .map(|t| t.name().to_string()),
        );
        names.extend(
            self.plugin_tools
                .iter()
                .flat_map(|(_id, tools)| tools.iter())
                .filter(|tool| self.session_allows_dynamic(tool.name()))
                .map(|t| t.name().to_string()),
        );
        names
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolUseContext;
    use crate::tool_search_view::ToolRegistryView;
    use crate::tool_trait::{
        DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    };
    use async_trait::async_trait;
    use permission::result::PermissionMetadata;
    use permission::{PermissionDecisionReason, PermissionResult};
    use serde_json::json;

    struct DummyTool;

    #[async_trait]
    impl Tool for DummyTool {
        fn name(&self) -> &str {
            "Dummy"
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({"type": "object"}));
            &SCHEMA
        }
        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            true
        }
        fn is_read_only(&self, _input: &serde_json::Value) -> bool {
            true
        }
        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> PermissionResult {
            PermissionResult::Allow {
                reason: PermissionDecisionReason::Other {
                    reason: "test".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: PermissionMetadata::default(),
            }
        }
        async fn description(
            &self,
            _input: &serde_json::Value,
            _opts: &DescriptionOptions,
        ) -> String {
            "Dummy".into()
        }
        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolUseContext,
            _tx: crate::progress::ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            Ok(ToolCallResult {
                mcp_meta_projection: None,
                model_content_projection: None,
                data_projection: None,
                data: json!({"ok": true}),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
    }

    #[test]
    fn find_by_name_returns_registered_tool() {
        let mut r = ToolRegistry::new();
        r.register_builtin(Arc::new(DummyTool));
        assert!(r.find_by_name("Dummy").is_some());
        assert!(r.find_by_name("Nonexistent").is_none());
    }

    #[test]
    fn find_registered_sees_allowlisted_hidden_builtins() {
        let mut r = ToolRegistry::new();
        r.register_builtin(Arc::new(DummyTool));
        r.set_session_tool_allowlist(&["Bash".to_string()]);
        assert!(r.find_by_name("Dummy").is_none());
        assert!(r.find_registered("Dummy").is_some());
        assert!(r.find_registered("Nonexistent").is_none());
    }

    /// A stub tool with a configurable name (for ordering tests).
    struct NamedTool(&'static str);

    #[async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({"type": "object"}));
            &SCHEMA
        }
        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            true
        }
        fn is_read_only(&self, _input: &serde_json::Value) -> bool {
            true
        }
        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> PermissionResult {
            PermissionResult::Allow {
                reason: PermissionDecisionReason::Other {
                    reason: "test".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: PermissionMetadata::default(),
            }
        }
        async fn description(
            &self,
            _input: &serde_json::Value,
            _opts: &DescriptionOptions,
        ) -> String {
            self.0.into()
        }
        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolUseContext,
            _tx: crate::progress::ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            Ok(ToolCallResult {
                mcp_meta_projection: None,
                model_content_projection: None,
                data_projection: None,
                data: json!({"ok": true}),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
    }

    /// `available_tools` emits builtins in claude-code's `localeCompare` order
    /// (NOT registration order, NOT byte order): `ListMcpResourcesTool` before
    /// `LSP`, and `REPL` LAST after `RemoteTrigger` — the exact divergent pairs
    /// from `assembleToolPool`'s `name.localeCompare` (`tools.ts:345-367`).
    #[test]
    fn available_tools_orders_builtins_by_localecompare() {
        let mut r = ToolRegistry::new();
        // Register in a deliberately scrambled order.
        for n in [
            "REPL",
            "LSP",
            "Read",
            "RemoteTrigger",
            "ListMcpResourcesTool",
            "ReadMcpResourceTool",
        ] {
            r.register_builtin(Arc::new(NamedTool(n)));
        }
        let names: Vec<String> = r
            .available_tools(&ToolStaticContext::default())
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert_eq!(
            names,
            vec![
                "ListMcpResourcesTool",
                "LSP",
                "Read",
                "ReadMcpResourceTool",
                "RemoteTrigger",
                "REPL",
            ]
        );
        // Distinguish from a byte sort, which would put LSP first & REPL early.
        let mut byte_sorted = names.clone();
        byte_sorted.sort_unstable();
        assert_ne!(names, byte_sorted, "must be localeCompare, not byte order");
    }

    /// MCP / plugin partitions are insertion-ordered (Vec, not HashMap) and are
    /// emitted AFTER the builtin prefix, each locale-sorted; a dynamic tool
    /// whose name collides with a builtin is dropped (builtin-precedence dedup).
    #[test]
    fn available_tools_partitions_builtin_prefix_then_dynamic() {
        let mut r = ToolRegistry::new();
        r.register_builtin(Arc::new(NamedTool("Write")));
        r.register_builtin(Arc::new(NamedTool("Bash")));
        let conn = McpConnectionId::new();
        r.register_mcp_tools(
            conn,
            vec![
                Arc::new(NamedTool("mcp__b")) as Arc<dyn Tool>,
                Arc::new(NamedTool("mcp__a")) as Arc<dyn Tool>,
                // Collides with a builtin — must be dropped (uniqBy precedence).
                Arc::new(NamedTool("Bash")) as Arc<dyn Tool>,
            ],
        );
        let names: Vec<String> = r
            .available_tools(&ToolStaticContext::default())
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        // Builtin prefix (locale-sorted), then MCP (locale-sorted); no dup Bash.
        assert_eq!(names, vec!["Bash", "Write", "mcp__a", "mcp__b"]);
    }

    #[test]
    fn restricted_filter_denies_code_running_builtins_but_preserves_dynamic_tools() {
        let mut r = ToolRegistry::new();
        for name in ["Bash", "PowerShell", "REPL", "WebFetch", "Read", "Edit"] {
            r.register_builtin(Arc::new(NamedTool(name)));
        }
        let conn = McpConnectionId::new();
        r.register_mcp_tools(
            conn,
            vec![Arc::new(NamedTool("mcp__srv__run")) as Arc<dyn Tool>],
        );
        r.set_restricted_builtin_filter(None);
        let names: Vec<String> = r
            .available_tools(&ToolStaticContext::default())
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();
        assert_eq!(names, vec!["Edit", "Read", "mcp__srv__run"]);
        assert!(r.find_by_name("Bash").is_none());
        assert!(r.find_by_name("mcp__srv__run").is_some());

        // `--tools` is a complete built-in allowlist, so an explicit restore
        // brings back only the named built-ins without exposing the rest.
        r.set_restricted_builtin_filter(Some(&["Bash".to_string(), " WebFetch,Read ".to_string()]));
        let names: Vec<String> = r
            .available_tools(&ToolStaticContext::default())
            .iter()
            .map(|tool| tool.name().to_string())
            .collect();
        assert_eq!(names, vec!["Bash", "Read", "WebFetch", "mcp__srv__run"]);
    }

    #[test]
    fn default_selection_restores_only_composition_catalog_and_preserves_mcp() {
        let mut registry = ToolRegistry::new();
        for name in ["Bash", "Edit", "Read", "Agent"] {
            registry.register_builtin(Arc::new(NamedTool(name)));
        }
        registry.set_builtin_catalog(&["Bash", "Edit", "Read"]);
        registry.register_mcp_tools(
            McpConnectionId::new(),
            vec![Arc::new(NamedTool("mcp__fixture__tool"))],
        );
        registry.set_restricted_builtin_filter(Some(&[String::new()]));
        assert!(registry.mod_registration_disabled());
        registry.set_restricted_builtin_filter(Some(&["default".into()]));
        assert!(!registry.mod_registration_disabled());
        let names: Vec<_> = registry
            .available_tools(&ToolStaticContext::default())
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect();
        assert_eq!(names, ["Bash", "Edit", "Read", "mcp__fixture__tool"]);
        assert!(registry.find_by_name("Agent").is_none());
    }

    #[test]
    fn required_builtin_survives_empty_selection_without_enabling_mods() {
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(NamedTool("Bash")));
        registry.register_required_builtin(Arc::new(NamedTool("StructuredOutput")));
        registry.set_restricted_builtin_filter(Some(&[String::new()]));
        assert!(registry.mod_registration_disabled());
        assert!(registry.find_by_name("Bash").is_none());
        assert!(registry.find_by_name("StructuredOutput").is_some());
        let names: Vec<_> = registry
            .available_tools(&ToolStaticContext::default())
            .iter()
            .map(|tool| tool.name().to_owned())
            .collect();
        assert_eq!(names, ["StructuredOutput"]);

        registry.set_session_tool_allowlist(&[]);
        assert!(registry.find_by_name("StructuredOutput").is_none());
        assert!(
            registry
                .available_tools(&ToolStaticContext::default())
                .is_empty()
        );
    }

    #[test]
    fn session_allowlist_filters_schema_lookup_names_and_dynamic_tools() {
        let mut r = ToolRegistry::new();
        r.register_builtin(Arc::new(NamedTool("Read")));
        r.register_builtin(Arc::new(NamedTool("Write")));
        r.register_mcp_tools(
            McpConnectionId::new(),
            vec![Arc::new(NamedTool("mcp__srv__mutate")) as Arc<dyn Tool>],
        );
        r.set_session_tool_allowlist(&["Read".to_string(), "mcp__srv__mutate".to_string()]);

        let names = r
            .available_tools(&ToolStaticContext::default())
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["Read"]);
        assert_eq!(r.all_names(), vec!["Read"]);
        assert!(r.find_by_name("Read").is_some());
        assert!(r.find_by_name("Write").is_none());
        assert!(r.find_by_name("mcp__srv__mutate").is_none());
    }

    /// A stub tool with a configurable name + `should_defer` + `search_hint`,
    /// for the Tool Search deferred-view tests.
    struct DeferNamedTool {
        name: &'static str,
        defer: bool,
        hint: Option<&'static str>,
    }

    #[async_trait]
    impl Tool for DeferNamedTool {
        fn name(&self) -> &str {
            self.name
        }
        fn search_hint(&self) -> Option<&str> {
            self.hint
        }
        fn should_defer(&self) -> bool {
            self.defer
        }
        fn input_schema(&self) -> &serde_json::Value {
            static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
                once_cell::sync::Lazy::new(|| json!({"type": "object"}));
            &SCHEMA
        }
        fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
            true
        }
        fn max_result_size_chars(&self) -> usize {
            1024
        }
        fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
            true
        }
        fn is_read_only(&self, _input: &serde_json::Value) -> bool {
            true
        }
        async fn check_permissions(
            &self,
            _input: &serde_json::Value,
            _ctx: &ToolUseContext,
        ) -> PermissionResult {
            PermissionResult::Allow {
                reason: PermissionDecisionReason::Other {
                    reason: "test".into(),
                },
                updated_input: None,
                update_destination: None,
                metadata: PermissionMetadata::default(),
            }
        }
        async fn description(
            &self,
            _input: &serde_json::Value,
            _opts: &DescriptionOptions,
        ) -> String {
            self.name.into()
        }
        async fn prompt(&self, _opts: &PromptOptions) -> String {
            String::new()
        }
        async fn call(
            &self,
            _input: serde_json::Value,
            _ctx: ToolUseContext,
            _tx: crate::progress::ToolProgressSender,
        ) -> Result<ToolCallResult, ToolError> {
            Ok(ToolCallResult {
                mcp_meta_projection: None,
                model_content_projection: None,
                data_projection: None,
                data: json!({"ok": true}),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            })
        }
    }

    /// Default (disabled) deferral ⇒ `refresh_tool_search_view` yields an EMPTY
    /// view even when a `should_defer` tool is registered — the correct behavior
    /// for a non-tool-search session, and byte-identical to the empty static view.
    #[test]
    fn tool_search_view_empty_when_deferral_disabled() {
        let mut r = ToolRegistry::new();
        r.register_builtin(Arc::new(DeferNamedTool {
            name: "Task",
            defer: true,
            hint: None,
        }));
        r.refresh_tool_search_view();
        assert!(r.tool_search_view().is_empty());
    }

    /// The empty-view REGRESSION: with tool search enabled, the refreshed view is
    /// the DEFERRED set (should_defer tools only) — not empty, and not the
    /// non-deferred tools. Kills the former "always returns no results" behavior.
    #[test]
    fn tool_search_view_is_deferred_set_when_enabled() {
        let mut r = ToolRegistry::new();
        r.register_builtin(Arc::new(DeferNamedTool {
            name: "Read",
            defer: false,
            hint: None,
        }));
        r.register_builtin(Arc::new(DeferNamedTool {
            name: "Task",
            defer: true,
            hint: Some("run a subagent"),
        }));
        r.set_deferral(Arc::new(crate::defer::DeferralState::new(
            crate::defer::ToolSearchMode::Enabled,
            false,
        )));
        r.refresh_tool_search_view();
        let names: Vec<String> = r
            .tool_search_view()
            .entries()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["Task".to_string()], "only should_defer tools");
        // The searchHint is carried through for scoring.
        let entry = r.tool_search_view().entries().into_iter().next().unwrap();
        assert_eq!(entry.search_hint.as_deref(), Some("run a subagent"));
    }

    /// An MCP tool registered after boot (before the refresh) appears in the
    /// deferred view — the static-snapshot fix must include MCP tools, per the
    /// dossier's composition-root requirement.
    #[test]
    fn tool_search_view_includes_deferred_mcp_tool() {
        let mut r = ToolRegistry::new();
        r.set_deferral(Arc::new(crate::defer::DeferralState::new(
            crate::defer::ToolSearchMode::Enabled,
            false,
        )));
        let conn = McpConnectionId::new();
        r.register_mcp_tools(
            conn,
            vec![Arc::new(DeferNamedTool {
                name: "mcp__srv__do",
                defer: true,
                hint: None,
            }) as Arc<dyn Tool>],
        );
        r.refresh_tool_search_view();
        let names: Vec<String> = r
            .tool_search_view()
            .entries()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["mcp__srv__do".to_string()]);
    }

    #[test]
    fn session_allowlist_keeps_denied_tools_out_of_tool_search() {
        let mut r = ToolRegistry::new();
        r.set_deferral(Arc::new(crate::defer::DeferralState::new(
            crate::defer::ToolSearchMode::Enabled,
            false,
        )));
        r.register_builtin(Arc::new(DeferNamedTool {
            name: "Read",
            defer: true,
            hint: None,
        }));
        r.register_builtin(Arc::new(DeferNamedTool {
            name: "Write",
            defer: true,
            hint: None,
        }));
        r.register_mcp_tools(
            McpConnectionId::new(),
            vec![Arc::new(DeferNamedTool {
                name: "mcp__srv__mutate",
                defer: true,
                hint: None,
            }) as Arc<dyn Tool>],
        );
        r.refresh_tool_search_view();
        assert!(
            r.tool_search_view()
                .entries()
                .iter()
                .any(|entry| entry.name == "Write")
        );
        r.set_session_tool_allowlist(&["Read".to_string()]);

        let names = r
            .tool_search_view()
            .entries()
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["Read"]);
    }

    /// A tool loaded via `ToolSearch` remains searchable; Claude Code allows an
    /// already-discovered schema to be selected again.
    #[test]
    fn tool_search_view_keeps_loaded_tool() {
        let mut r = ToolRegistry::new();
        r.register_builtin(Arc::new(DeferNamedTool {
            name: "Task",
            defer: true,
            hint: None,
        }));
        let defer = Arc::new(crate::defer::DeferralState::new(
            crate::defer::ToolSearchMode::Enabled,
            false,
        ));
        r.set_deferral(defer.clone());
        r.refresh_tool_search_view();
        assert_eq!(r.tool_search_view().len(), 1);
        // Simulate a ToolSearch load, then refresh again.
        defer.mark_loaded(["Task".to_string()]);
        r.refresh_tool_search_view();
        assert_eq!(r.tool_search_view().len(), 1);
    }

    /// Re-registering through the shared registry upserts in place; unregister
    /// drops it. This is the runtime `tools/list_changed` mutation path.
    #[test]
    fn shared_mcp_register_is_order_preserving_upsert() {
        let r = Arc::new(ToolRegistry::new());
        let conn = McpConnectionId::new();
        r.register_mcp_tools(conn, vec![Arc::new(NamedTool("mcp__x")) as Arc<dyn Tool>]);
        assert!(r.find_by_name("mcp__x").is_some());
        // Upsert replaces the tool set for the same id.
        r.register_mcp_tools(conn, vec![Arc::new(NamedTool("mcp__y")) as Arc<dyn Tool>]);
        assert!(r.find_by_name("mcp__x").is_none());
        assert!(r.find_by_name("mcp__y").is_some());
        r.unregister_mcp_tools(conn);
        assert!(r.find_by_name("mcp__y").is_none());
    }

    #[test]
    fn replace_mcp_tools_swaps_entire_partition_set_for_lag_recovery() {
        let r = Arc::new(ToolRegistry::new());
        r.register_mcp_tools(
            McpConnectionId::new(),
            vec![Arc::new(NamedTool("mcp__keep")) as Arc<dyn Tool>],
        );
        let replacement = McpConnectionId::new();
        r.replace_mcp_tools(vec![(
            replacement,
            vec![
                Arc::new(NamedTool("ListMcpResourcesTool")) as Arc<dyn Tool>,
                Arc::new(NamedTool("ReadMcpResourceTool")) as Arc<dyn Tool>,
                Arc::new(NamedTool("ReadMcpResourceDirTool")) as Arc<dyn Tool>,
            ],
        )]);

        assert!(r.find_by_name("mcp__keep").is_none());
        assert!(r.find_by_name("ListMcpResourcesTool").is_some());
        assert!(r.find_by_name("ReadMcpResourceTool").is_some());
        assert!(r.find_by_name("ReadMcpResourceDirTool").is_some());
    }

    #[test]
    fn mod_tool_survives_mcp_catalog_rebuild_until_owner_unloads() {
        let registry = ToolRegistry::new();
        registry.register_mod_tool("example", Arc::new(NamedTool("mcp__example__run")));
        registry.replace_mcp_tools(Vec::new());
        assert!(registry.find_by_name("mcp__example__run").is_some());
        registry.unregister_mod_tools("example");
        assert!(registry.find_by_name("mcp__example__run").is_none());

        let mut empty = ToolRegistry::new();
        empty.set_restricted_builtin_filter(Some(&[]));
        assert!(empty.mod_registration_disabled());
    }
}
