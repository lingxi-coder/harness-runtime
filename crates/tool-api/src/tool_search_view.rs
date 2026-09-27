//! Registry view surface consumed by the `ToolSearch` tool.
//!
//! `ToolSearch` (claude-code `ToolSearchTool`) searches the set of DEFERRED
//! tools — the tools serialized on the wire with `defer_loading: true` — and
//! returns the ones matching a query so the model can pull their full schemas
//! into context. To search, it needs a read-only view of those entries.
//!
//! These types live in `tool-api` (not in `tools/meta`) so the [`ToolRegistry`]
//! can OWN the live view cell and refresh it from its own deferred tool set
//! (see [`ToolRegistry::refresh_tool_search_view`]). `tools/meta` re-uses this
//! surface for the actual scoring implementation.
//!
//! [`ToolRegistry`]: crate::registry::ToolRegistry
//! [`ToolRegistry::refresh_tool_search_view`]: crate::registry::ToolRegistry::refresh_tool_search_view

use std::sync::RwLock;

/// One row in the searchable registry view.
#[derive(Debug, Clone)]
pub struct ToolSearchEntry {
    /// Tool name (used for ranking + select/exact/prefix matching).
    pub name: String,
    /// Tool description (lower-signal token source, scored at +2).
    pub description: String,
    /// Curated capability phrase (`tool.searchHint`), scored at +4. claude-code
    /// scores `searchHint` separately from (and higher than) the prompt-derived
    /// description.
    pub search_hint: Option<String>,
}

/// Read-only snapshot of the searchable (deferred) tool set fed to
/// `ToolSearchTool`. Avoids the `Arc<ToolRegistry>` cycle that would arise from
/// the tool holding a strong ref to its owning registry.
pub trait ToolRegistryView: Send + Sync {
    /// All searchable tool entries (name + description + search hint).
    fn entries(&self) -> Vec<ToolSearchEntry>;
}

/// Static-vec implementation — used by tests and any snapshot-only path.
pub struct StaticRegistryView {
    entries: Vec<ToolSearchEntry>,
}

impl StaticRegistryView {
    /// Construct from a vec of entries.
    #[must_use]
    pub fn new(entries: Vec<ToolSearchEntry>) -> Self {
        Self { entries }
    }
}

impl ToolRegistryView for StaticRegistryView {
    fn entries(&self) -> Vec<ToolSearchEntry> {
        self.entries.clone()
    }
}

/// A live, mutable view cell shared between the [`ToolRegistry`] (the writer,
/// via [`ToolRegistry::refresh_tool_search_view`]) and the `ToolSearchTool` (the
/// reader). The composition root builds the registry, then refreshes this cell
/// with the deferred set — INCLUDING MCP tools registered at boot — killing the
/// former "empty static view always returns no results" behavior.
///
/// Empty by default, so a session with tool search disabled (the default) has an
/// empty deferred set and `ToolSearch` correctly returns nothing.
///
/// [`ToolRegistry`]: crate::registry::ToolRegistry
/// [`ToolRegistry::refresh_tool_search_view`]: crate::registry::ToolRegistry::refresh_tool_search_view
#[derive(Default)]
pub struct SharedToolSearchView {
    entries: RwLock<Vec<ToolSearchEntry>>,
}

impl SharedToolSearchView {
    /// Construct an empty shared view.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(Vec::new()),
        }
    }

    /// Replace the searchable entry set (called by the registry refresh).
    pub fn set_entries(&self, entries: Vec<ToolSearchEntry>) {
        *self
            .entries
            .write()
            .expect("tool-search view lock poisoned") = entries;
    }

    /// Number of searchable entries currently held (test/telemetry helper).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries
            .read()
            .expect("tool-search view lock poisoned")
            .len()
    }

    /// Whether the view is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ToolRegistryView for SharedToolSearchView {
    fn entries(&self) -> Vec<ToolSearchEntry> {
        self.entries
            .read()
            .expect("tool-search view lock poisoned")
            .clone()
    }
}
