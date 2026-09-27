//! `tengu_plugin_*` event schemas (2.1.251 byte-alignment §20c).
//!
//! # ⚠️ SUBSTRATE ONLY — none of these 24 events is emitted anywhere yet
//!
//! This module defines names and payload shapes. **It has no production
//! emit site.** A repo-wide
//! `grep -rn --include='*.rs' -e 'tengu::plugin' -e 'tengu_plugin'` matches
//! only this file, `tengu/mod.rs`, and two test files: no crate outside
//! `telemetry` references the module at all. `ALL_EVENT_NAMES` has no
//! runtime consumer (only tests read it), so nothing misbehaves — but the
//! registry advertises 24 plugin events that cannot appear in any telemetry
//! stream, and §20c is therefore **OPEN, not closed**. Do not read the
//! registered names, the count-lock test, or the parity fixture as evidence
//! that plugin telemetry ships.
//!
//! The nearest miss is `tengu_plugin_uninstalled_cli`: the oracle's `LIn`
//! (@166666396) emits it on the line immediately before `QWn(se.orphans,P)`,
//! and `QWn` IS ported (as `uninstall_orphan_suffix`,
//! `apps/cli/src/commands/plugin_install.rs`) — the emit two statements
//! earlier was walked past. Wiring the emit sites is deliberately deferred
//! to a task that owns those call sites rather than guessed at from here.
//!
//! `tengu_plugin_enabled_for_session` is the flagship event (fired once per
//! loaded plugin, every session) and is fully traced against the oracle
//! below. Its four siblings that are ALSO fully traced
//! ([`NameCollisionPayload`], [`FolderShadowedPayload`], [`RenamedPayload`],
//! [`LoadFailedPayload`]) get their own payload structs; the remaining CLI
//! lifecycle events are registered by name only — minting a payload shape
//! from a guess rather than the actual call site risks locking in a wrong
//! wire shape (the §20b lesson from `tengu::mcp`'s `ToolsListedPayload`).
//!
//! ## The shared `_PROTO_*` / redacted identity fields
//!
//! Every identity-bearing event below spreads the oracle's `q1(name,
//! marketplace, …)` helper, which in turn spreads `y5t(name, marketplace,
//! …)`. Traced verbatim from the 2.1.251 binary:
//!
//! ```text
//! function y5t(e,t,r=null){
//!   let o=B1(e,t,r), u=h5t(o)||Wmt(e,t);
//!   return {
//!     plugin_id_hash: nk(e,t),
//!     plugin_scope: c(o),
//!     plugin_name_redacted: u ? e : Jw,
//!     marketplace_name_redacted: u && t ? t : Jw,
//!     is_official_plugin: j1(o),
//!   };
//! }
//! function q1(e,t,r=null){
//!   let o = yme(e,t) ?? t;
//!   return {
//!     _PROTO_plugin_name: e,
//!     ...(o && { _PROTO_marketplace_name: o }),
//!     ...y5t(e,t,r),
//!   };
//! }
//! ```
//!
//! So every one of these events carries BOTH the redacted/hashed fields the
//! byte-alignment doc named (`plugin_id_hash`, `plugin_name_redacted`,
//! `marketplace_name_redacted`) AND two raw, un-redacted `_PROTO_*` fields
//! (`_PROTO_plugin_name`, `_PROTO_marketplace_name`) the doc's §20c summary
//! did not call out — this is the crate's `PiiTagged` case exactly (routed to
//! privileged `BigQuery` proto columns, stripped by
//! [`crate::pii::strip_proto_fields`] for any sink that isn't proto-aware).
//! There is no shared Rust struct for this identity block: `#[serde(flatten)]`
//! and `#[serde(deny_unknown_fields)]` cannot be combined on the same
//! container (a hard serde restriction), and this module's convention
//! mandates `deny_unknown_fields` on every payload struct, so the six fields
//! are duplicated verbatim into each event that spreads them — matching the
//! oracle's actual (flattened) wire shape rather than nesting them under a
//! synthetic key.
//!
//! ## `enabled_via` — the byte-alignment doc's value set does not hold
//!
//! §20c lists `enabled_via (org-policy / auto_install / admin-install /
//! seed-mount)`. Traced to its producer (`zin`, called as `c(me)` at the
//! `tengu_plugin_enabled_for_session` call site):
//!
//! ```text
//! function zin(e,t,r){
//!   if(e.isBuiltin) return "default-enable";
//!   if(t?.has(e.name)) return "org-policy";
//!   if(e.installationPreference==="required"
//!      || e.installationPreference==="auto_install") return "admin-install";
//!   if(r.some(o=>e.path.startsWith(...))) return "seed-mount";
//!   return "user-install";
//! }
//! ```
//!
//! `auto_install` is an *input* (`installationPreference`) the function
//! *tests*, never an `enabled_via` *output* — the real five-member output set
//! is `default-enable` / `org-policy` / `admin-install` / `seed-mount` /
//! `user-install` (all consistently kebab-case, so [`EnabledVia`] uses a
//! single `rename_all = "kebab-case"` rather than per-variant renames).

use crate::pii::{PiiTagged, Verified};
use serde::{Deserialize, Serialize};

/// `tengu_plugin_enabled_for_session` — emitted once per loaded, enabled
/// plugin at session start.
pub const ENABLED_FOR_SESSION: &str = "tengu_plugin_enabled_for_session";
/// `tengu_plugin_name_collision` — two or more plugin-declared components
/// (skills, commands, agents) resolved to the same name.
pub const NAME_COLLISION: &str = "tengu_plugin_name_collision";
/// `tengu_plugin_folder_shadowed` — a plugin's on-disk folder was shadowed by
/// a same-named entry from another source.
pub const FOLDER_SHADOWED: &str = "tengu_plugin_folder_shadowed";
/// `tengu_plugin_renamed` — a plugin's canonical name changed across a
/// resolve chain (rename-chain resolution outcome).
pub const RENAMED: &str = "tengu_plugin_renamed";
/// `tengu_plugin_load_failed` — a plugin or marketplace failed to load.
pub const LOAD_FAILED: &str = "tengu_plugin_load_failed";
/// `tengu_plugin_installed` — a plugin install completed (non-CLI path).
pub const INSTALLED: &str = "tengu_plugin_installed";
/// `tengu_plugin_installed_cli` — `claude plugin install` completed.
pub const INSTALLED_CLI: &str = "tengu_plugin_installed_cli";
/// `tengu_plugin_uninstalled_cli` — `claude plugin uninstall` completed.
pub const UNINSTALLED_CLI: &str = "tengu_plugin_uninstalled_cli";
/// `tengu_plugin_enabled_cli` — `claude plugin enable` completed.
pub const ENABLED_CLI: &str = "tengu_plugin_enabled_cli";
/// `tengu_plugin_disabled_cli` — `claude plugin disable` completed.
pub const DISABLED_CLI: &str = "tengu_plugin_disabled_cli";
/// `tengu_plugin_disabled_all_cli` — `claude plugin disable --all` (or
/// equivalent) completed.
pub const DISABLED_ALL_CLI: &str = "tengu_plugin_disabled_all_cli";
/// `tengu_plugin_updated_cli` — `claude plugin update` completed.
pub const UPDATED_CLI: &str = "tengu_plugin_updated_cli";
/// `tengu_plugin_command_failed` — a `claude plugin …` CLI subcommand
/// errored.
pub const COMMAND_FAILED: &str = "tengu_plugin_command_failed";
/// `tengu_plugin_remote_fetch` — a marketplace/plugin archive fetch over the
/// network (git or direct download).
pub const REMOTE_FETCH: &str = "tengu_plugin_remote_fetch";
/// `tengu_plugin_install_command` — the install CLI surface was invoked.
pub const INSTALL_COMMAND: &str = "tengu_plugin_install_command";
/// `tengu_plugin_uninstall_command` — the uninstall CLI surface was invoked.
pub const UNINSTALL_COMMAND: &str = "tengu_plugin_uninstall_command";
/// `tengu_plugin_enable_command` — the enable CLI surface was invoked.
pub const ENABLE_COMMAND: &str = "tengu_plugin_enable_command";
/// `tengu_plugin_disable_command` — the disable CLI surface was invoked.
pub const DISABLE_COMMAND: &str = "tengu_plugin_disable_command";
/// `tengu_plugin_update_command` — the update CLI surface was invoked.
pub const UPDATE_COMMAND: &str = "tengu_plugin_update_command";
/// `tengu_plugin_list_command` — the list CLI surface was invoked.
pub const LIST_COMMAND: &str = "tengu_plugin_list_command";
/// `tengu_plugin_details_command` — the details CLI surface was invoked.
pub const DETAILS_COMMAND: &str = "tengu_plugin_details_command";
/// `tengu_plugin_prune_command` — the prune CLI surface was invoked.
pub const PRUNE_COMMAND: &str = "tengu_plugin_prune_command";
/// `tengu_plugin_prune_cli` — prune removed one or more orphaned plugins.
pub const PRUNE_CLI: &str = "tengu_plugin_prune_cli";
/// `tengu_plugin_state_file_error` — the installed-plugin durable state file
/// could not be read, parsed, or normalized cleanly.
pub const STATE_FILE_ERROR: &str = "tengu_plugin_state_file_error";

/// Registry block — order is locked (append-only). Consumed by
/// [`crate::tengu::ALL_EVENT_NAMES`].
pub const NAMES: &[&str] = &[
    ENABLED_FOR_SESSION,
    NAME_COLLISION,
    FOLDER_SHADOWED,
    RENAMED,
    LOAD_FAILED,
    INSTALLED,
    INSTALLED_CLI,
    UNINSTALLED_CLI,
    ENABLED_CLI,
    DISABLED_CLI,
    DISABLED_ALL_CLI,
    UPDATED_CLI,
    COMMAND_FAILED,
    REMOTE_FETCH,
    INSTALL_COMMAND,
    UNINSTALL_COMMAND,
    ENABLE_COMMAND,
    DISABLE_COMMAND,
    UPDATE_COMMAND,
    LIST_COMMAND,
    DETAILS_COMMAND,
    PRUNE_COMMAND,
    PRUNE_CLI,
    STATE_FILE_ERROR,
];

/// How a plugin came to be enabled for this session. See the module doc:
/// the oracle's real five-member set, NOT the byte-alignment doc's guess.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum EnabledVia {
    /// Built into the host; always enabled.
    DefaultEnable,
    /// Named in an org-managed enable policy.
    OrgPolicy,
    /// `installationPreference` was `"required"` or `"auto_install"`.
    AdminInstall,
    /// Loaded from a seed-mounted plugin directory.
    SeedMount,
    /// Explicitly enabled by the user.
    UserInstall,
}

/// Payload for [`ENABLED_FOR_SESSION`]. Field order follows the oracle's
/// object-literal source order, not the byte-alignment doc's prose order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginEnabledForSessionPayload {
    /// Raw (un-redacted) plugin name — PII-tagged proto column.
    #[serde(rename = "_PROTO_plugin_name")]
    pub proto_plugin_name: PiiTagged,
    /// Raw (un-redacted) marketplace name, when known.
    #[serde(
        rename = "_PROTO_marketplace_name",
        skip_serializing_if = "Option::is_none"
    )]
    pub proto_marketplace_name: Option<PiiTagged>,
    /// Stable hash of `(plugin_name, marketplace_name)`.
    pub plugin_id_hash: Verified,
    /// Resolved install scope (user / project / managed / …).
    pub plugin_scope: Verified,
    /// The plugin's display name, or a fixed placeholder when untrusted.
    pub plugin_name_redacted: Verified,
    /// The marketplace's display name, or a fixed placeholder when untrusted.
    pub marketplace_name_redacted: Verified,
    /// Whether this plugin ships from an Anthropic-official marketplace.
    pub is_official_plugin: bool,
    /// Present only when the plugin declares a `serverPluginId`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_plugin_id: Option<Verified>,
    /// How the plugin came to be enabled.
    pub enabled_via: EnabledVia,
    /// Present only when the manifest declares an `installationPreference`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installation_preference: Option<Verified>,
    /// Count of skill paths the plugin contributes.
    pub skill_path_count: u32,
    /// Count of command paths the plugin contributes.
    pub command_path_count: u32,
    /// Count of agent paths the plugin contributes.
    pub agent_path_count: u32,
    /// `!skipMcpDiscovery && mcpServers !== undefined`.
    pub has_mcp: bool,
    /// `skipMcpDiscovery === true` — the plugin's MCP servers are host-owned.
    pub host_owned_mcp: bool,
    /// Whether the manifest declares `lspServers`.
    pub has_lsp: bool,
    /// Whether the manifest declares `hooksConfig`.
    pub has_hooks: bool,
    /// Whether the manifest declares `settings`.
    pub has_settings: bool,
    /// Sessions elapsed since this plugin was last used, when the host
    /// surfaced a last-use record for this plugin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions_since_last_use: Option<u32>,
    /// Days elapsed since this plugin was last used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days_since_last_use: Option<u32>,
    /// Whether the plugin is running under safe mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safe_mode: Option<bool>,
    /// Sorted, comma-joined settings key list, when the plugin declares any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings_keys: Option<Verified>,
    /// Normalized manifest version, when declared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<Verified>,
    /// Number of skills this plugin contributes, by name.
    ///
    /// Oracle `T5t(e,t)` (@159542839), spread into this event:
    /// ```text
    /// function T5t(e,t){
    ///   if(e===null)return{};
    ///   let r=e.get(t)??[];
    ///   return{ skill_name_hash_count:r.length,
    ///           ...r.length>0&&{skill_name_hashes:Vo(r.map((o)=>HR(o)).sort().join(","))} }
    /// }
    /// ```
    /// `e` is `null` only for a builtin/official plugin (`S5t(M)?null:o`), so
    /// every OTHER plugin carries `skill_name_hash_count` — including the
    /// zero case. `None` here models the builtin/official arm, where the
    /// oracle spreads `{}` and neither key appears.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_name_hash_count: Option<u32>,
    /// Hash of the plugin's sorted, comma-joined skill names — present only
    /// when [`Self::skill_name_hash_count`] is `Some(n)` with `n > 0`
    /// (oracle: `...r.length>0&&{…}`). Independently corroborated as a real
    /// payload key by the oracle's PII-strip key list (@156128816).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_name_hashes: Option<Verified>,
}

/// Payload for [`NAME_COLLISION`]. Does NOT spread the `q1` identity block
/// (this event classifies a component-name collision across sources, not one
/// plugin's identity).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NameCollisionPayload {
    /// Component kind that collided (`skill` / `command` / `agent`).
    pub item_type: Verified,
    /// Raw (un-redacted) colliding component name — PII-tagged proto column.
    #[serde(rename = "_PROTO_skill_name")]
    pub proto_skill_name: PiiTagged,
    /// Hash of the colliding component name.
    pub item_name_hash: Verified,
    /// Number of distinct sources that declared this name.
    pub source_count: u32,
    /// Sorted, comma-joined list of the colliding sources.
    pub sources: Verified,
    /// The source that won the collision, when a resolution occurred.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub winner_source: Option<Verified>,
}

/// Payload for [`FOLDER_SHADOWED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FolderShadowedPayload {
    /// Which manifest component was shadowed.
    pub component: Verified,
    /// Raw (un-redacted) plugin name — PII-tagged proto column.
    #[serde(rename = "_PROTO_plugin_name")]
    pub proto_plugin_name: PiiTagged,
    /// Raw (un-redacted) marketplace name, when known.
    #[serde(
        rename = "_PROTO_marketplace_name",
        skip_serializing_if = "Option::is_none"
    )]
    pub proto_marketplace_name: Option<PiiTagged>,
    /// Stable hash of `(plugin_name, marketplace_name)`.
    pub plugin_id_hash: Verified,
    /// Resolved install scope.
    pub plugin_scope: Verified,
    /// The plugin's display name, or a fixed placeholder when untrusted.
    pub plugin_name_redacted: Verified,
    /// The marketplace's display name, or a fixed placeholder when untrusted.
    pub marketplace_name_redacted: Verified,
    /// Whether this plugin ships from an Anthropic-official marketplace.
    pub is_official_plugin: bool,
}

/// Payload for [`RENAMED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenamedPayload {
    /// Rename-chain resolution outcome (e.g. `resolved` / `unresolved`).
    pub outcome: Verified,
    /// Depth of the rename chain, when the outcome resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chain_depth: Option<u32>,
    /// Why resolution stopped, when the outcome was `unresolved`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<Verified>,
    /// Raw (un-redacted) plugin name — PII-tagged proto column.
    #[serde(rename = "_PROTO_plugin_name")]
    pub proto_plugin_name: PiiTagged,
    /// Raw (un-redacted) marketplace name, when known.
    #[serde(
        rename = "_PROTO_marketplace_name",
        skip_serializing_if = "Option::is_none"
    )]
    pub proto_marketplace_name: Option<PiiTagged>,
    /// Stable hash of `(plugin_name, marketplace_name)`.
    pub plugin_id_hash: Verified,
    /// Resolved install scope.
    pub plugin_scope: Verified,
    /// The plugin's display name, or a fixed placeholder when untrusted.
    pub plugin_name_redacted: Verified,
    /// The marketplace's display name, or a fixed placeholder when untrusted.
    pub marketplace_name_redacted: Verified,
    /// Whether this plugin ships from an Anthropic-official marketplace.
    pub is_official_plugin: bool,
}

/// Payload for [`LOAD_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadFailedPayload {
    /// The loader error record's `type` DISCRIMINANT — oracle
    /// `s("tengu_plugin_load_failed",{error_category:c(o.type),…})` (`OWn`,
    /// @159545727). Values are record kinds such as `generic-error`
    /// (@162860362), `synced-plugin-shadowed` (@162862227) and
    /// `marketplace-load-failed` (which `OWn` tests for on its very next
    /// line, to decide the `untrustedReservedName` redaction branch).
    ///
    /// **NOT the `policy`/`network`/`not-found`/`permission`/`validation`/
    /// `unknown` ladder.** An earlier revision documented that value set
    /// here; it belongs to `G1(e)` (@159544700), a SEPARATE function feeding
    /// a different event. Classifying with the ladder would make this
    /// dimension uncomparable with the oracle's and would leave the
    /// `untrustedReservedName` branch — which keys off
    /// `o.type === "marketplace-load-failed"` — with no value to key on.
    pub error_category: Verified,
    /// Whether this failure was resolved from a cache-only lookup.
    pub cache_only: bool,
    /// Which stage/component failed, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component: Option<Verified>,
    /// The underlying error code, when the failure carried one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errno: Option<Verified>,
    /// Raw (un-redacted) plugin name — PII-tagged proto column.
    #[serde(rename = "_PROTO_plugin_name")]
    pub proto_plugin_name: PiiTagged,
    /// Raw (un-redacted) marketplace name, when known.
    #[serde(
        rename = "_PROTO_marketplace_name",
        skip_serializing_if = "Option::is_none"
    )]
    pub proto_marketplace_name: Option<PiiTagged>,
    /// Stable hash of `(plugin_name, marketplace_name)`.
    pub plugin_id_hash: Verified,
    /// Resolved install scope. Forced to `user-local` for the
    /// untrusted-reserved-name marketplace-load-failed case.
    pub plugin_scope: Verified,
    /// The plugin's display name, or a fixed placeholder when untrusted.
    pub plugin_name_redacted: Verified,
    /// The marketplace's display name, or a fixed placeholder when untrusted.
    pub marketplace_name_redacted: Verified,
    /// Whether this plugin ships from an Anthropic-official marketplace.
    pub is_official_plugin: bool,
}
