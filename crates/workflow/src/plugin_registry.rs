//! Live, cross-crate table of plugin-declared workflow scripts.
//!
//! `plugin.json`'s `workflows` field (a `union([path, path[]])` over
//! directories/`.js` files, or the `workflows/` auto-scan when the field is
//! absent) is parsed by `plugin::discovery` into
//! `PluginComponents::workflows`, but nothing materializes it: a plugin's
//! saved workflow is invisible to the `Workflow` tool's by-name resolver.
//!
//! Oracle (`2.1.251`, the plugin-workflow loader `v()`/`P()` in the bundled
//! JS): every plugin workflow is namespaced `${pluginName}:${meta.name}` —
//! `meta.name` parsed from the script's own `export const meta = {…}` block
//! — and joins the SAME lookup table as built-in / project / user workflows.
//!
//! The oracle's precedence is **project/user > plugin > built-in**, and the
//! plugin/built-in half is a REAL shadow, not a no-op. `j()` (@169051841)
//! seeds the map from the built-ins and then overwrites:
//! `let c=wWe(), e=new Map(c.map(k=>[k.name,k])), r=O(m,e); for(let k of r)
//! e.set(k.name,k)`, and the final array is
//! `[...c.filter(k=>!d.has(k.name)), ...u, ...l]` — unshadowed built-ins,
//! then plugins not shadowed by project/user, then project/user. The only
//! thing that stops a plugin record from displacing a same-named built-in is
//! its own script failing to parse (`W(o,s){if(!s||BBn(o.script))return!0;…}`,
//! `O(o,s){return o.filter(t=>W(t,s.get(t.name)))}` @169052203).
//!
//! The port's `name` resolver (`tool_workflow::resolve_script_at`,
//! `tools/workflow/src/lib.rs`) already checks saved (project/user) workflows,
//! then this registry, then the built-in table last — matching the oracle
//! order above. Local App workflows are plugin-owned as of Phase 9 and are
//! never duplicated in `tools/workflow`'s `BUILTIN_WORKFLOWS`, which today
//! holds only the generic `deep-research` workflow
//! (`tools/workflow/src/builtins.rs`); every key in this registry contains a
//! `:` (namespacing is unconditional in `plugin::manager`) and no built-in
//! name does, so the two orders cannot disagree on any reachable input
//! regardless.
//!
//! Namespacing likewise means a project/user collision only happens if such a
//! file is itself literally named `<plugin>:<name>.js`.
//!
//! LingXi's built-in/project/user tiers are all addressed by FILENAME, not
//! by parsed script metadata (`workflow::meta_string_value` is used only for
//! display/telemetry, never for lookup) — a wider, pre-existing divergence
//! from the oracle that this module does not attempt to fix. A PLUGIN
//! workflow, though, is addressed exactly as the oracle addresses it: by its
//! own `meta.name`, with no filename fallback. `v()` (@169045500) drops any
//! file whose meta does not parse (`if("error"in r) return warn(`Plugin
//! workflow ${o} has invalid meta: ${r.error} — skipping`), null`) or which
//! is not a regular file of at most [`MAX_WORKFLOW_SCRIPT_BYTES`], so a
//! shared helper module sitting in `workflows/` never becomes a workflow
//! name. Falling back to the file stem there would put a name in the
//! `Workflow` tool's `Available:` list that then fails `validate_meta` inside
//! the launcher — an accept-then-fail the oracle cannot produce.
//!
//! This type lives in `workflow` (not `plugin`) because `plugin` is not — and
//! must not become — a dependency of `tool-workflow` or `tasks` (the
//! consumers): `apps/cli`/`apps/engine-desktop` are the only crates
//! depending on `plugin` today. `workflow` is the one crate every consumer
//! already shares (`tool-workflow` and `tasks` both depend on it, and
//! `plugin` gains it as a new, acyclic dependency here), so a composition
//! root can construct ONE `Arc<PluginWorkflowRegistry>`, hand it to
//! `plugin::PluginManager::with_plugin_workflows`, and hand the SAME `Arc` to
//! `tool_workflow::WorkflowTool::with_plugin_workflows` /
//! `tasks::handlers::local_workflow::LocalWorkflowHandler::with_plugin_workflows`
//! — without either resolver crate depending on `plugin` itself.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

/// Largest `.js` a plugin workflow may be before the loader drops it.
///
/// Oracle `um = 524288` (@156964916), applied by `v()` through
/// `ZI(c,o,um)` — a file over the cap is never read into a workflow record,
/// so an oversized script cannot reach the registry (and the resolver
/// therefore never needs its own cap).
pub const MAX_WORKFLOW_SCRIPT_BYTES: u64 = 524_288;

/// One plugin-declared workflow script, keyed by its already-namespaced name.
#[derive(Debug, Clone)]
pub struct PluginWorkflowEntry {
    /// `{plugin}:{name}` — see the module doc for how `{name}` is derived.
    pub name: String,
    /// Absolute path to the workflow's `.js` source.
    pub script_path: PathBuf,
}

/// Shared table of plugin-declared workflow scripts, addressable by name
/// alongside built-in / project / user workflows.
///
/// `register`/`unregister` are keyed by the plugin's own bookkeeping (the
/// caller — `PluginManager` — tracks which names belong to which plugin, the
/// same shape as its existing `plugin_mcp_names` / `plugin_agent_names`
/// maps), so this type itself only ever sees flat name lists.
#[derive(Debug, Default)]
pub struct PluginWorkflowRegistry {
    by_name: RwLock<HashMap<String, PathBuf>>,
}

impl PluginWorkflowRegistry {
    /// A new, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert (or overwrite) a batch of entries — one plugin's full
    /// contribution, called once per `load_plugin`. A name collision across
    /// two plugins is last-write-wins (matching the oracle's `Map.set`
    /// during discovery); the resolution-order guarantee that matters is
    /// project/user over plugin, enforced by the CALLER checking this
    /// registry only after the project/user directories have already missed.
    pub fn register(&self, entries: Vec<PluginWorkflowEntry>) {
        if entries.is_empty() {
            return;
        }
        let mut guard = self.by_name.write().unwrap_or_else(|e| e.into_inner());
        for entry in entries {
            guard.insert(entry.name, entry.script_path);
        }
    }

    /// Remove exactly the named entries (a single plugin's set, tracked by
    /// the caller) — the symmetric counterpart to `register`, called from
    /// `unload_plugin`.
    pub fn unregister(&self, names: &[String]) {
        if names.is_empty() {
            return;
        }
        let mut guard = self.by_name.write().unwrap_or_else(|e| e.into_inner());
        for name in names {
            guard.remove(name);
        }
    }

    /// Resolve a namespaced workflow name to its script path.
    #[must_use]
    pub fn resolve(&self, name: &str) -> Option<PathBuf> {
        let guard = self.by_name.read().unwrap_or_else(|e| e.into_inner());
        guard.get(name).cloned()
    }

    /// All currently registered names, sorted — for the "Available: …" name
    /// listing alongside built-in/project/user names.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let guard = self.by_name.read().unwrap_or_else(|e| e.into_inner());
        let mut names: Vec<String> = guard.keys().cloned().collect();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_resolve_round_trips() {
        let registry = PluginWorkflowRegistry::new();
        registry.register(vec![PluginWorkflowEntry {
            name: "acme:deploy".to_string(),
            script_path: PathBuf::from("/plugins/acme/workflows/deploy.js"),
        }]);
        assert_eq!(
            registry.resolve("acme:deploy"),
            Some(PathBuf::from("/plugins/acme/workflows/deploy.js"))
        );
        assert_eq!(registry.resolve("acme:missing"), None);
    }

    #[test]
    fn unregister_removes_exactly_the_named_entries() {
        let registry = PluginWorkflowRegistry::new();
        registry.register(vec![
            PluginWorkflowEntry {
                name: "acme:deploy".to_string(),
                script_path: PathBuf::from("/a/deploy.js"),
            },
            PluginWorkflowEntry {
                name: "acme:rollback".to_string(),
                script_path: PathBuf::from("/a/rollback.js"),
            },
            PluginWorkflowEntry {
                name: "other:keep".to_string(),
                script_path: PathBuf::from("/b/keep.js"),
            },
        ]);
        registry.unregister(&["acme:deploy".to_string(), "acme:rollback".to_string()]);
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
        registry.register(vec![
            PluginWorkflowEntry {
                name: "zeta:z".to_string(),
                script_path: PathBuf::from("/z.js"),
            },
            PluginWorkflowEntry {
                name: "alpha:a".to_string(),
                script_path: PathBuf::from("/a.js"),
            },
        ]);
        assert_eq!(
            registry.names(),
            vec!["alpha:a".to_string(), "zeta:z".to_string()]
        );
    }
}
