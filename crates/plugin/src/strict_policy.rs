//! Strict-plugin-only policy.
//!
//! Host policy can lock individual component slots so that only plugin
//! sources may contribute them. A locked slot rejects user/project-level
//! settings, dynamic configs, etc. — the only acceptable source is a
//! plugin that has been admin-approved.
//!
//! See spec §15.6.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// A component category that can be locked by the strict policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[allow(missing_docs)]
pub enum PluginComponent {
    Commands,
    Agents,
    Skills,
    Hooks,
    OutputStyles,
    McpServers,
    LspServers,
    Channels,
}

/// Strict-plugin-only policy.
///
/// A component appearing in `locked` is only allowed to be contributed by
/// a plugin source — manual `claude` config, project files, etc. are
/// ignored for that slot.
pub struct StrictPluginOnlyPolicy {
    /// Components that must come from a plugin source only.
    pub locked: HashSet<PluginComponent>,
}

impl StrictPluginOnlyPolicy {
    /// Empty policy — no components are locked.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            locked: HashSet::new(),
        }
    }

    /// Return `true` when `c` is locked (plugin sources only).
    #[must_use]
    pub fn is_locked(&self, c: PluginComponent) -> bool {
        self.locked.contains(&c)
    }

    /// Resolve `strictPluginOnlyCustomization` from settings tiers ordered
    /// lowest to highest priority. A later declaration replaces the earlier
    /// one, matching the scalar managed-settings merge.
    #[must_use]
    pub fn from_settings_tiers<'a>(tiers: impl IntoIterator<Item = &'a str>) -> Self {
        let mut resolved: Option<HashSet<PluginComponent>> = None;
        for raw in tiers {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
                continue;
            };
            let Some(setting) = value.get("strictPluginOnlyCustomization") else {
                continue;
            };
            let next = match setting {
                serde_json::Value::Bool(true) => all_components(),
                serde_json::Value::Bool(false) | serde_json::Value::Null => HashSet::new(),
                serde_json::Value::Array(slots) => slots
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .filter_map(component_from_slot)
                    .collect(),
                _ => {
                    tracing::warn!(
                        "strictPluginOnlyCustomization must be a boolean or component array; ignoring invalid tier"
                    );
                    continue;
                }
            };
            resolved = Some(next);
        }
        Self {
            locked: resolved.unwrap_or_default(),
        }
    }
}

fn all_components() -> HashSet<PluginComponent> {
    [
        PluginComponent::Agents,
        PluginComponent::Skills,
        PluginComponent::Hooks,
        PluginComponent::McpServers,
    ]
    .into_iter()
    .collect()
}

fn component_from_slot(slot: &str) -> Option<PluginComponent> {
    Some(match slot {
        "agents" => PluginComponent::Agents,
        "skills" => PluginComponent::Skills,
        "hooks" => PluginComponent::Hooks,
        "mcp" => PluginComponent::McpServers,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_tiers_resolve_mcp_lock_with_last_tier_wins() {
        let policy = StrictPluginOnlyPolicy::from_settings_tiers([
            r#"{"strictPluginOnlyCustomization":true}"#,
            r#"{"strictPluginOnlyCustomization":["mcp","hooks"]}"#,
        ]);
        assert!(policy.is_locked(PluginComponent::McpServers));
        assert!(policy.is_locked(PluginComponent::Hooks));
        assert!(!policy.is_locked(PluginComponent::Commands));
    }

    #[test]
    fn boolean_true_locks_only_the_four_customization_surfaces() {
        let policy = StrictPluginOnlyPolicy::from_settings_tiers([
            r#"{"strictPluginOnlyCustomization":true}"#,
        ]);

        assert_eq!(policy.locked.len(), 4);
        assert!(policy.is_locked(PluginComponent::Agents));
        assert!(policy.is_locked(PluginComponent::Skills));
        assert!(policy.is_locked(PluginComponent::Hooks));
        assert!(policy.is_locked(PluginComponent::McpServers));
        assert!(!policy.is_locked(PluginComponent::Commands));
        assert!(!policy.is_locked(PluginComponent::OutputStyles));
        assert!(!policy.is_locked(PluginComponent::LspServers));
        assert!(!policy.is_locked(PluginComponent::Channels));
    }

    #[test]
    fn unknown_and_non_customization_array_entries_are_ignored() {
        let policy = StrictPluginOnlyPolicy::from_settings_tiers([
            r#"{"strictPluginOnlyCustomization":["skills","commands","outputStyles","unknown"]}"#,
        ]);

        assert_eq!(policy.locked, HashSet::from([PluginComponent::Skills]));
    }

    #[test]
    fn mcp_servers_alias_is_not_recognized_the_oracle_enum_is_mcp_only() {
        // Oracle: $pe = ["skills","agents","hooks","mcp"]; the array form is
        // pre-filtered by `r.filter(c => $pe.includes(c))`, so "mcpServers"
        // is dropped upstream and never locks anything. This port must match:
        // an unrecognized slot name locks nothing.
        let policy = StrictPluginOnlyPolicy::from_settings_tiers([
            r#"{"strictPluginOnlyCustomization":["mcpServers"]}"#,
        ]);

        assert!(!policy.is_locked(PluginComponent::McpServers));
        assert!(policy.locked.is_empty());
    }

    #[test]
    fn explicit_false_unlocks_previous_tier() {
        let policy = StrictPluginOnlyPolicy::from_settings_tiers([
            r#"{"strictPluginOnlyCustomization":["mcp"]}"#,
            r#"{"strictPluginOnlyCustomization":false}"#,
        ]);
        assert!(!policy.is_locked(PluginComponent::McpServers));
    }
}
