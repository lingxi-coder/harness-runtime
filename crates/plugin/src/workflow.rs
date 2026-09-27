//! Compatibility helpers for the pre-registry plugin workflow API.
//!
//! Production workflow registration lives in [`workflow::PluginWorkflowRegistry`]
//! and is wired by [`crate::manager::PluginManager`]. This module deliberately
//! contains no resolver or lifecycle state; the two small helpers below keep
//! the branch's existing desktop discovery regression test source-compatible
//! while it migrates to the shared registry.

use crate::manifest::ComponentPath;
use std::path::PathBuf;

/// One discovered plugin workflow for the legacy discovery assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowInventoryEntry {
    /// Contributing plugin manifest name.
    pub plugin_name: String,
    /// Discovered script path.
    pub path: PathBuf,
    /// Script-declared `meta.name` when valid.
    pub meta_name: Option<String>,
    /// Namespaced plugin workflow name when `meta.name` is present.
    pub fqn: Option<String>,
}

/// Parse a workflow's authoritative literal `meta.name` using the shared
/// workflow runtime parser; this is not a second JavaScript parser.
#[must_use]
pub fn extract_meta_name(script: &str) -> Option<String> {
    workflow::meta_string_value(script, "name")
}

/// Build the legacy display inventory from already-discovered component paths.
/// The live production path uses `PluginManager::with_plugin_workflows`; this
/// helper is retained only for the desktop discovery regression test.
pub async fn build_plugin_workflow_inventory(
    plugin_name: &str,
    components: &[ComponentPath],
) -> Vec<WorkflowInventoryEntry> {
    let mut entries = Vec::with_capacity(components.len());
    for component in components {
        let meta_name = match tokio::fs::read_to_string(&component.path).await {
            Ok(source) => extract_meta_name(&source),
            Err(_) => None,
        };
        let fqn = meta_name
            .as_deref()
            .map(|name| format!("{plugin_name}:{name}"));
        entries.push(WorkflowInventoryEntry {
            plugin_name: plugin_name.to_string(),
            path: component.path.clone(),
            meta_name,
            fqn,
        });
    }
    entries
}
