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

use lingxi_core::host::SourceToolWireDefinition;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::sync::{Arc, RwLock};

type EntryFilter = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// One row in the searchable registry view.
#[derive(Debug, Clone)]
pub struct ToolSearchEntry {
    /// Tool name (used for ranking + select/exact/prefix matching).
    pub name: String,
    /// Tool description (lower-signal token source, scored at +2).
    pub description: Utf16JsonProjection,
    /// Curated capability phrase (`tool.searchHint`), scored at +4. claude-code
    /// scores `searchHint` separately from (and higher than) the prompt-derived
    /// description.
    pub search_hint: Option<Utf16JsonProjection>,
}

/// A single searchable catalog generation and its exact advertised definitions.
/// Definitions originate in the filtered request wire snapshot, never in a
/// later registry lookup while replaying an earlier ToolSearch result.
#[derive(Debug, Clone, Default)]
pub struct ToolSearchSnapshot {
    /// Search candidates and their ranking text from one catalog generation.
    pub entries: Vec<ToolSearchEntry>,
    /// Exact filtered definitions available when that generation was prepared.
    pub definitions: Vec<SourceToolWireDefinition>,
}

/// Read-only snapshot of the searchable (deferred) tool set fed to
/// `ToolSearchTool`. Avoids the `Arc<ToolRegistry>` cycle that would arise from
/// the tool holding a strong ref to its owning registry.
pub trait ToolRegistryView: Send + Sync {
    /// All searchable tool entries (name + description + search hint).
    fn entries(&self) -> Vec<ToolSearchEntry>;

    /// Capture names and definitions together. Entry-only views retain their
    /// historical result shape and do not claim to provide frozen schemas.
    fn snapshot(&self) -> ToolSearchSnapshot {
        ToolSearchSnapshot {
            entries: self.entries(),
            definitions: Vec::new(),
        }
    }
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
    snapshot: RwLock<ToolSearchSnapshot>,
    entry_filter: RwLock<Option<EntryFilter>>,
}

impl SharedToolSearchView {
    /// Construct an empty shared view.
    #[must_use]
    pub fn new() -> Self {
        Self {
            snapshot: RwLock::new(ToolSearchSnapshot::default()),
            entry_filter: RwLock::new(None),
        }
    }

    /// Install a live authority check so revocation also invalidates materialized entries.
    pub fn set_entry_filter(&self, filter: EntryFilter) {
        *self
            .entry_filter
            .write()
            .expect("tool-search filter lock poisoned") = Some(filter);
    }

    /// Replace the searchable entry set (called by the registry refresh).
    pub fn set_entries(&self, entries: Vec<ToolSearchEntry>) {
        self.set_snapshot(ToolSearchSnapshot {
            entries,
            definitions: Vec::new(),
        });
    }

    /// Publish both parts under one lock so discovery cannot combine names
    /// from one refresh with definitions from another.
    pub fn set_snapshot(&self, snapshot: ToolSearchSnapshot) {
        *self
            .snapshot
            .write()
            .expect("tool-search view lock poisoned") = snapshot;
    }

    fn capture(&self, include_definitions: bool) -> ToolSearchSnapshot {
        let filter = self
            .entry_filter
            .read()
            .expect("tool-search filter lock poisoned");
        let snapshot = self
            .snapshot
            .read()
            .expect("tool-search view lock poisoned");
        let entries: Vec<_> = snapshot
            .entries
            .iter()
            .filter(|entry| filter.as_ref().is_none_or(|filter| filter(&entry.name)))
            .cloned()
            .collect();
        let definitions = if include_definitions {
            let names: std::collections::HashSet<_> =
                entries.iter().map(|entry| entry.name.as_str()).collect();
            snapshot
                .definitions
                .iter()
                .filter(|definition| names.contains(definition.name.as_str()))
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        ToolSearchSnapshot {
            entries,
            definitions,
        }
    }

    /// Number of searchable entries currently held (test/telemetry helper).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries().len()
    }

    /// Whether the view is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ToolRegistryView for SharedToolSearchView {
    fn entries(&self) -> Vec<ToolSearchEntry> {
        self.capture(false).entries
    }

    fn snapshot(&self) -> ToolSearchSnapshot {
        self.capture(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn entry(name: &str, description: &str) -> ToolSearchEntry {
        ToolSearchEntry {
            name: name.into(),
            description: description.into(),
            search_hint: None,
        }
    }

    fn definition(name: &str, description: &str) -> SourceToolWireDefinition {
        SourceToolWireDefinition {
            name: name.into(),
            description: description.into(),
            input_schema: Utf16JsonProjection::parse(r#"{"type":"object"}"#).unwrap(),
            strict: false,
            defer_loading: true,
        }
    }

    fn catalog(description: &str) -> ToolSearchSnapshot {
        ToolSearchSnapshot {
            entries: vec![entry("Read", description)],
            definitions: vec![definition("Read", description)],
        }
    }

    #[test]
    fn captured_definitions_survive_catalog_replacement_and_entry_only_refresh_clears_them() {
        let view = SharedToolSearchView::new();
        view.set_snapshot(catalog("original schema"));
        let captured = view.snapshot();
        view.set_snapshot(catalog("replacement schema"));

        assert_eq!(
            captured.entries[0].description,
            captured.definitions[0].description
        );
        assert_eq!(
            captured.definitions[0].description.string_at(""),
            Some("original schema")
        );
        assert_eq!(
            view.snapshot().definitions[0].description.string_at(""),
            Some("replacement schema")
        );

        view.set_entries(vec![entry("Write", "entry-only refresh")]);
        let current = view.snapshot();
        assert_eq!(current.entries[0].name, "Write");
        assert!(current.definitions.is_empty());
        assert_eq!(captured.definitions[0].name, "Read");
    }

    #[test]
    fn concurrent_refreshes_never_mix_entry_and_definition_generations() {
        let view = Arc::new(SharedToolSearchView::new());
        view.set_snapshot(catalog("initial"));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let writer = {
            let view = view.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                for generation in 0..512 {
                    view.set_snapshot(catalog(&format!("generation {generation}")));
                    std::thread::yield_now();
                }
            })
        };
        barrier.wait();
        for _ in 0..1024 {
            let captured = view.snapshot();
            assert_eq!(captured.entries.len(), 1);
            assert_eq!(captured.definitions.len(), 1);
            assert_eq!(captured.entries[0].name, captured.definitions[0].name);
            assert_eq!(
                captured.entries[0].description,
                captured.definitions[0].description
            );
            std::thread::yield_now();
        }
        writer.join().unwrap();
    }

    #[test]
    fn live_revocation_filters_entries_and_definitions_from_the_same_snapshot() {
        let view = SharedToolSearchView::new();
        view.set_snapshot(ToolSearchSnapshot {
            entries: vec![entry("Read", "read"), entry("Write", "write")],
            definitions: vec![definition("Read", "read"), definition("Write", "write")],
        });
        let allow_write = Arc::new(AtomicBool::new(true));
        view.set_entry_filter({
            let allow_write = allow_write.clone();
            Arc::new(move |name| name != "Write" || allow_write.load(Ordering::Acquire))
        });
        let captured = view.snapshot();
        assert_eq!(captured.definitions.len(), 2);
        allow_write.store(false, Ordering::Release);
        let revoked = view.snapshot();
        assert_eq!(
            revoked
                .entries
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Read"]
        );
        assert_eq!(
            revoked
                .definitions
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Read"]
        );
        assert_eq!(
            captured.definitions[1].name, "Write",
            "an earlier by-value result stays immutable"
        );
    }

    #[test]
    fn entry_only_views_have_no_frozen_definitions() {
        let view = StaticRegistryView::new(vec![entry("Read", "read")]);
        let captured = view.snapshot();
        assert_eq!(captured.entries[0].name, "Read");
        assert!(captured.definitions.is_empty());
    }
}
