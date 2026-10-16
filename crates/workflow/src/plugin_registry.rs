//! Live, ordered snapshot of plugin-declared workflow scripts.
//!
//! The fixed Claude Code 2.1.295 workflow loader discovers plugin workflows
//! from the current enabled-plugin list, namespaces each as
//! `{plugin}:{meta.name}`, and merges same-name candidates in order. A later
//! candidate replaces an existing workflow only when its full syntax check
//! succeeds. Keeping each loaded plugin's candidate set lets an unload rebuild
//! that same ordered view and reveal an earlier still-loaded valid candidate.
//! Names remain raw UTF-16 code units for identity and lookup.

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::PluginId;
use std::path::PathBuf;
use std::sync::RwLock;

/// Largest `.js` a plugin workflow may be before the loader drops it.
///
/// Oracle `um = 524288` (@156964916), applied by `v()` through
/// `ZI(c,o,um)` — a file over the cap is never read into a workflow record,
/// so an oversized script cannot reach the registry (and the resolver
/// therefore never needs its own cap).
pub const MAX_WORKFLOW_SCRIPT_BYTES: u64 = 524_288;

/// One plugin-declared workflow captured from the validated plugin load.
///
/// The exact name, script and owner are snapshotted together so downstream
/// workflow descriptor/launch code does not reopen a path after validation.
#[derive(Debug, Clone)]
pub struct PluginWorkflowEntry {
    /// `{plugin}:{meta.name}` as raw UTF-16, matching JavaScript string identity.
    pub qualified_name: Utf16JsonProjection,
    /// Native-decoded UTF-8 source snapshot. Invalid byte sequences are
    /// replaced with U+FFFD, matching the loader's Buffer-to-string decode.
    pub script: String,
    /// Canonical path of the workflow's `.js` source, retained for existing
    /// path-based resolvers.
    pub script_path: PathBuf,
    /// Plugin name that supplied this workflow.
    pub plugin_name: String,
}

/// Shared ordered snapshot of plugin-declared workflow scripts, addressable
/// by name alongside built-in / project / user workflows. Each owner's full
/// ordered candidate set is retained so removing a later shadow rebuilds the
/// selected view from the still-loaded owners.
#[derive(Debug, Default)]
pub struct PluginWorkflowRegistry {
    state: RwLock<RegistryState>,
}

#[derive(Debug, Default)]
struct RegistryState {
    owners: Vec<OwnerWorkflowEntries>,
    visible: Vec<PluginWorkflowEntry>,
}

#[derive(Debug)]
struct OwnerWorkflowEntries {
    owner: PluginId,
    entries: Vec<PluginWorkflowEntry>,
}

impl RegistryState {
    fn rebuild_visible(&mut self) {
        let mut visible = Vec::<PluginWorkflowEntry>::new();
        for owner in &self.owners {
            for entry in &owner.entries {
                let units = entry
                    .qualified_name
                    .string_units("")
                    .expect("workflow qualified name is a root string");
                if let Some(existing) = visible.iter_mut().find(|existing| {
                    existing.qualified_name.string_units("").as_deref() == Some(units.as_slice())
                }) {
                    // Native retains the current candidate if a later same-name
                    // script fails the full parse. Non-colliding scripts remain
                    // eligible for the later catalog gate even when malformed.
                    if crate::validate_meta(&entry.script).is_ok() {
                        *existing = entry.clone();
                    }
                } else {
                    visible.push(entry.clone());
                }
            }
        }
        self.visible = visible;
    }
}

impl PluginWorkflowRegistry {
    /// A new, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace one loaded plugin owner's ordered workflow candidates. An empty
    /// set clears a prior snapshot for the same owner; it does not add a new
    /// empty owner. The effective registry is rebuilt using Native's ordered
    /// last-valid same-name selection.
    pub fn register(&self, owner: PluginId, entries: Vec<PluginWorkflowEntry>) {
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = state.owners.iter_mut().find(|item| item.owner == owner) {
            existing.entries = entries;
        } else if !entries.is_empty() {
            state.owners.push(OwnerWorkflowEntries { owner, entries });
        }
        state.rebuild_visible();
    }

    /// Remove one plugin's complete candidate set and reveal any still-loaded
    /// owner that supplied an earlier valid same-name workflow.
    pub fn unregister(&self, owner: PluginId) {
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        let before = state.owners.len();
        state.owners.retain(|item| item.owner != owner);
        if state.owners.len() != before {
            state.rebuild_visible();
        }
    }

    /// Resolve a workflow whose name is represented by a Rust string.
    /// Use [`Self::resolve_units`] when the caller has raw JavaScript UTF-16.
    #[must_use]
    pub fn resolve(&self, name: &str) -> Option<PathBuf> {
        self.resolve_units(&name.encode_utf16().collect::<Vec<_>>())
    }

    /// Resolve a workflow name by raw JavaScript string code units.
    #[must_use]
    pub fn resolve_units(&self, name: &[u16]) -> Option<PathBuf> {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        state
            .visible
            .iter()
            .find(|entry| entry.qualified_name.string_units("").as_deref() == Some(name))
            .map(|entry| entry.script_path.clone())
    }

    /// Snapshot all loaded plugin workflows in registry insertion order.
    ///
    /// Each entry contains the script and exact name that passed metadata
    /// discovery. Bodies may still need the full syntax gate before listing or
    /// execution.
    #[must_use]
    pub fn entries(&self) -> Vec<PluginWorkflowEntry> {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        state.visible.clone()
    }

    /// Display all registered names, sorted, for the "Available: …" listing.
    /// This is lossy for unpaired surrogates; use [`Self::entries`] for names
    /// used as identity.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        let mut names: Vec<String> = state
            .visible
            .iter()
            .map(|entry| {
                String::from_utf16_lossy(
                    &entry
                        .qualified_name
                        .string_units("")
                        .expect("qualified workflow name is a root string"),
                )
            })
            .collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, script_path: &str, script: &str) -> PluginWorkflowEntry {
        let units = name.encode_utf16().collect::<Vec<_>>();
        PluginWorkflowEntry {
            qualified_name: Utf16JsonProjection::root_string(
                String::from_utf16_lossy(&units),
                units,
            )
            .unwrap(),
            script: script.to_string(),
            script_path: PathBuf::from(script_path),
            plugin_name: "acme".to_string(),
        }
    }

    #[test]
    fn register_then_resolve_round_trips() {
        let registry = PluginWorkflowRegistry::new();
        let owner = PluginId::new();
        registry.register(
            owner,
            vec![entry(
                "acme:deploy",
                "/plugins/acme/workflows/deploy.js",
                "export const meta = { name: 'deploy' };",
            )],
        );
        assert_eq!(
            registry.resolve("acme:deploy"),
            Some(PathBuf::from("/plugins/acme/workflows/deploy.js"))
        );
        assert_eq!(registry.resolve("acme:missing"), None);
    }

    #[test]
    fn unregister_removes_only_the_named_owner_entries() {
        let registry = PluginWorkflowRegistry::new();
        let owner = PluginId::new();
        let keep_owner = PluginId::new();
        registry.register(
            owner,
            vec![
                entry(
                    "acme:deploy",
                    "/a/deploy.js",
                    "export const meta = { name: 'deploy', description: 'Deploy' };",
                ),
                entry(
                    "acme:rollback",
                    "/a/rollback.js",
                    "export const meta = { name: 'rollback', description: 'Rollback' };",
                ),
            ],
        );
        let mut keep = entry(
            "other:keep",
            "/b/keep.js",
            "export const meta = { name: 'keep', description: 'Keep' };",
        );
        keep.plugin_name = "other".to_string();
        registry.register(keep_owner, vec![keep]);
        registry.unregister(owner);
        assert_eq!(registry.resolve("acme:deploy"), None);
        assert_eq!(registry.resolve("acme:rollback"), None);
        assert_eq!(
            registry.resolve("other:keep"),
            Some(PathBuf::from("/b/keep.js"))
        );
    }

    #[test]
    fn names_are_sorted() {
        let registry = PluginWorkflowRegistry::new();
        let owner = PluginId::new();
        registry.register(
            owner,
            vec![
                entry("zeta:z", "/z.js", "z source"),
                entry("alpha:a", "/a.js", "a source"),
            ],
        );
        assert_eq!(
            registry.names(),
            vec!["alpha:a".to_string(), "zeta:z".to_string()]
        );
    }

    #[test]
    fn entries_preserve_registration_order_and_replace_the_snapshot_in_place() {
        let registry = PluginWorkflowRegistry::new();
        let owner = PluginId::new();
        let replacement_owner = PluginId::new();
        registry.register(
            owner,
            vec![
                entry(
                    "zeta:first",
                    "/z.js",
                    "export const meta = { name: 'first', description: 'd' };\nlog('old')",
                ),
                entry(
                    "alpha:second",
                    "/a.js",
                    "export const meta = { name: 'second', description: 'd' };\nlog('second')",
                ),
            ],
        );
        registry.register(
            replacement_owner,
            vec![entry(
                "zeta:first",
                "/z-v2.js",
                "export const meta = { name: 'first', description: 'd' };\nlog('replacement')",
            )],
        );

        let entries = registry.entries();
        assert_eq!(
            entries
                .iter()
                .map(|entry| {
                    String::from_utf16_lossy(&entry.qualified_name.string_units("").unwrap())
                })
                .collect::<Vec<_>>(),
            vec!["zeta:first".to_string(), "alpha:second".to_string()]
        );
        assert_eq!(
            entries[0].script,
            "export const meta = { name: 'first', description: 'd' };\nlog('replacement')"
        );
        assert_eq!(entries[0].script_path, PathBuf::from("/z-v2.js"));
        assert_eq!(
            entries[1].script,
            "export const meta = { name: 'second', description: 'd' };\nlog('second')"
        );
    }

    #[test]
    fn same_owner_reload_replaces_its_full_snapshot_without_moving_its_slot() {
        let registry = PluginWorkflowRegistry::new();
        let first_owner = PluginId::new();
        let second_owner = PluginId::new();
        registry.register(
            first_owner,
            vec![
                entry(
                    "acme:first",
                    "/first-v1.js",
                    "export const meta = { name: 'first' };\nlog('first-v1')",
                ),
                entry(
                    "acme:removed",
                    "/removed.js",
                    "export const meta = { name: 'removed' };",
                ),
            ],
        );
        registry.register(
            second_owner,
            vec![entry(
                "other:second",
                "/second.js",
                "export const meta = { name: 'second' };",
            )],
        );
        registry.register(
            first_owner,
            vec![entry(
                "acme:first",
                "/first-v2.js",
                "export const meta = { name: 'first' };\nlog('first-v2')",
            )],
        );

        let entries = registry.entries();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.script_path.clone())
                .collect::<Vec<_>>(),
            vec![PathBuf::from("/first-v2.js"), PathBuf::from("/second.js")]
        );
        assert_eq!(registry.resolve("acme:removed"), None);

        registry.register(first_owner, Vec::new());
        assert_eq!(registry.resolve("acme:first"), None);
        assert_eq!(
            registry.resolve("other:second"),
            Some(PathBuf::from("/second.js"))
        );
    }

    #[test]
    fn invalid_body_collision_keeps_the_prior_entry_and_does_not_claim_it() {
        let registry = PluginWorkflowRegistry::new();
        let first_owner = PluginId::new();
        let later_owner = PluginId::new();
        let prior = entry(
            "acme:deploy",
            "/plugins/valid.js",
            "export const meta = { name: 'deploy', description: 'd' };\nlog('ok')",
        );
        registry.register(first_owner, vec![prior.clone()]);

        let invalid_collision = entry(
            "acme:deploy",
            "/plugins/malformed.js",
            "export const meta = { name: 'deploy', description: 'd' };\nif (",
        );
        assert!(crate::validate_meta(&invalid_collision.script).is_err());
        registry.register(later_owner, vec![invalid_collision]);

        let entries = registry.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].script, prior.script);
        assert_eq!(entries[0].script_path, prior.script_path);
        assert_eq!(
            registry.resolve("acme:deploy"),
            Some(PathBuf::from("/plugins/valid.js"))
        );
    }

    #[test]
    fn noncolliding_invalid_body_is_retained_for_the_later_catalog_filter() {
        let registry = PluginWorkflowRegistry::new();
        let owner = PluginId::new();
        let invalid = entry(
            "acme:broken",
            "/plugins/broken.js",
            "export const meta = { name: 'broken', description: 'd' };\nif (",
        );
        assert!(crate::validate_meta(&invalid.script).is_err());
        registry.register(owner, vec![invalid.clone()]);
        let entries = registry.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].script, invalid.script);
        assert_eq!(entries[0].script_path, invalid.script_path);
    }

    #[test]
    fn unloading_a_valid_shadow_restores_the_still_loaded_owner() {
        let registry = PluginWorkflowRegistry::new();
        let first_owner = PluginId::new();
        let shadow_owner = PluginId::new();
        let first = entry(
            "acme:deploy",
            "/plugins/v1/deploy.js",
            "export const meta = { name: 'deploy', description: 'Deploy v1' };\nlog('v1')",
        );
        let shadow = entry(
            "acme:deploy",
            "/plugins/v2/deploy.js",
            "export const meta = { name: 'deploy', description: 'Deploy v2' };\nlog('v2')",
        );

        registry.register(first_owner, vec![first]);
        registry.register(shadow_owner, vec![shadow]);
        assert_eq!(
            registry.resolve("acme:deploy"),
            Some(PathBuf::from("/plugins/v2/deploy.js"))
        );

        registry.unregister(shadow_owner);
        assert_eq!(
            registry.resolve("acme:deploy"),
            Some(PathBuf::from("/plugins/v1/deploy.js"))
        );
        registry.unregister(first_owner);
        assert_eq!(registry.resolve("acme:deploy"), None);
    }

    #[test]
    fn malformed_later_shadow_does_not_replace_or_hide_an_earlier_valid_owner() {
        let registry = PluginWorkflowRegistry::new();
        let first_owner = PluginId::new();
        let later_owner = PluginId::new();
        registry.register(
            first_owner,
            vec![entry(
                "acme:deploy",
                "/plugins/valid.js",
                "export const meta = { name: 'deploy' };\nlog('valid')",
            )],
        );
        registry.register(
            later_owner,
            vec![entry(
                "acme:deploy",
                "/plugins/invalid.js",
                "export const meta = { name: 'deploy' };\nif (",
            )],
        );
        assert_eq!(
            registry.resolve("acme:deploy"),
            Some(PathBuf::from("/plugins/valid.js"))
        );
        registry.unregister(later_owner);
        assert_eq!(
            registry.resolve("acme:deploy"),
            Some(PathBuf::from("/plugins/valid.js"))
        );
    }

    #[test]
    fn raw_utf16_names_remain_distinct_for_lookup_and_unregistration() {
        let registry = PluginWorkflowRegistry::new();
        let owner = PluginId::new();
        let lone_owner = PluginId::new();
        let lone_surrogate = Utf16JsonProjection::root_string("�".into(), vec![0xd800]).unwrap();
        let replacement_character = Utf16JsonProjection::from("�");
        registry.register(
            owner,
            vec![PluginWorkflowEntry {
                qualified_name: replacement_character.clone(),
                script: "replacement-character".into(),
                script_path: PathBuf::from("/replacement.js"),
                plugin_name: "acme".into(),
            }],
        );
        registry.register(
            lone_owner,
            vec![PluginWorkflowEntry {
                qualified_name: lone_surrogate.clone(),
                script: "lone-surrogate".into(),
                script_path: PathBuf::from("/lone.js"),
                plugin_name: "acme".into(),
            }],
        );

        assert_eq!(
            registry.resolve_units(&[0xd800]),
            Some(PathBuf::from("/lone.js"))
        );
        assert_eq!(
            registry.resolve_units(&[0xfffd]),
            Some(PathBuf::from("/replacement.js"))
        );
        registry.unregister(lone_owner);
        assert_eq!(registry.resolve_units(&[0xd800]), None);
        assert_eq!(
            registry.resolve_units(&[0xfffd]),
            Some(PathBuf::from("/replacement.js"))
        );
    }
}
