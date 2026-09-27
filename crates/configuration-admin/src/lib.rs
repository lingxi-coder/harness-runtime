//! Shared file-backed configuration management for desktop and mobile hosts.

// Documentation debt, not a decision that docs do not matter: this crate had
// 126 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]
// Dead code kept visible, not swept: this crate had 11 item(s) rustc could
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

pub mod config_admin;
pub mod hook_admin;
pub mod mcp_admin;
pub mod mcp_bridge;
pub mod plugin_admin;
pub mod plugin_download;
pub mod plugin_install;
pub mod plugin_marketplace;
pub mod plugin_policy;
pub mod plugin_prune;
pub mod plugin_settings;
pub mod plugin_telemetry;
pub mod settings_bridge;
pub mod skills_admin;
