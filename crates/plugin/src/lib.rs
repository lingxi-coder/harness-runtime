//! Plugin subsystem — manifest model, 7-state lifecycle, strict policy,
//! agent-frontmatter privilege validation, and the 8-registry materialiser.
//!
//! Plugins are the most cross-cutting subsystem in the engine: every
//! component slot they declare (commands / agents / skills / hooks /
//! output styles / MCP servers / LSP servers / workflows) eventually lands in a
//! dedicated registry built by Plans 03 / 04 / 09 / 12. This crate owns
//! the lifecycle and the materialiser; Plan 16 wires the actual fetches.
//!
//! See spec §15 (Plugin System).

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 26 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]
// Dead code kept visible, not swept: this crate had 3 item(s) rustc could
// reach from nothing when the workspace was measured (2026-09-16). The lint
// stays `warn` at the workspace level so a NEW crate still inherits it; this
// allow is scoped here so the count is per crate and repayable by deleting this
// line. This is the category where "named, computed, never wired" hides — some
// of these read like features that were built and never connected. Each wants a
// decision (delete, or wire), not a blanket deletion.
// ⚠️ The count above is ONE macOS, lib-target measurement. It is not a list of
// deletable items — see docs/HANDOFF-dead-code-adjudication-2026-09-17.md,
// which records two near-misses where it said "dead" about live code.
#![allow(dead_code)]

#[cfg(test)]
fn plugin_seed_env_lock() -> &'static std::sync::Mutex<()> {
    use std::sync::{Mutex, OnceLock};

    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub mod agent_validation;
pub mod brand_normalize;
mod command_source;
pub mod dependency;
pub mod discovery;
mod git;
pub mod installed;
pub mod lifecycle;
pub mod loader;
pub mod manager;
pub mod manifest;
pub mod marketplace;
mod mcpb;
pub mod source;
pub mod strict_policy;
pub mod theme_registry;
pub mod trust;
pub mod workflow;
/// `${user_config.KEY}` substitution + plugin-option env helpers.
///
/// Re-exported from the `hooks` crate, which owns the single source of truth
/// (the plugin-hook executor needs the same substitution/gate logic at spawn
/// time, and `hooks` sits below `plugin` in the dependency graph). `plugin`'s
/// MCP/LSP loader continues to consume it as `plugin::user_config::*`.
pub use hooks::user_config;

pub use agent_validation::{validate_plugin_agent_frontmatter, AgentValidationError};
/// §19.1 / P0a.7 — brand-token normalization layer for comparing Claude-oracle
/// plugin fixtures against LingXi's plugin contract; see the module doc for
/// the frozen-identity vs. known-pair distinction.
pub use brand_normalize::{
    known_pairs, load_frozen_identities, normalize, BrandPair, FrozenIdentity, NormalizeReport,
};
pub use command_source::materialize_command_plugin_source;
pub use dependency::{
    merge_dependency_requirements, parse_dependencies, version_satisfies_all, PluginDependency,
};
pub use discovery::{
    cli_plugin_dir_collection_children, discover_cli_plugin_dirs, discover_effective_plugins,
    discover_enabled_plugins, discover_installed_plugins, discover_recorded_plugins,
    has_control_or_bidi_formatting, validate_marketplace_name, validate_plugin_name,
};
pub use git::{
    clone_plugin_git, clone_plugin_git_pinned, is_confusable_authority_url, is_suspicious_url,
};
pub use lifecycle::PluginState;
pub use loader::{resolve_user_config, LoaderError};
pub use manager::{PluginManager, PluginManagerError};
pub use manifest::{
    ComponentPath, PluginChannel, PluginComponents, PluginManifest, PluginUserConfig,
    UserConfigField, UserConfigSchema,
};
pub use marketplace::MarketplaceManager;
/// Normalize an extracted MCP bundle into the shared plugin manifest layout.
pub use mcpb::ensure_plugin_manifest;
pub use mcpb::prepare_extract_dir;
/// Stable content hash used for deterministic external-source cache keys.
pub use mcpb::sha256_hex as plugin_source_sha256;
/// Guarded zip extraction used by both installed MCP bundles and session-only
/// `--plugin-url` archives.
pub use mcpb::unpack_mcpb as unpack_plugin_archive;
pub use source::PluginSource;
pub use strict_policy::{PluginComponent, StrictPluginOnlyPolicy};
pub use theme_registry::{PluginThemeEntry, PluginThemeRegistry};
pub use trust::{default_trust_for_source, PluginTrustLevel};
/// Legacy discovery assertion helper. Production workflow registration uses
/// the shared `workflow::PluginWorkflowRegistry` instead.
pub use workflow::{build_plugin_workflow_inventory, extract_meta_name, WorkflowInventoryEntry};
