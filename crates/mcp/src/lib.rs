//! MCP (Model Context Protocol) connection management for `LingXi` Core.
//!
//! Owns the per-connection state machine (`connection.rs`), the
//! [`registry::McpRegistry`] that drives transitions through a
//! platform-supplied [`platform_api::McpTransport`], the OAuth 2.1
//! handshake skeleton (`oauth.rs`), the per-project MCP-server enable/
//! disable and project-`.mcp.json`-approval gate (`server_gate.rs`), and the
//! `client::McpClient` JSON-RPC client built on top of `lingxi-jsonrpc`
//! (M2-02b — see `docs/superpowers/plans/2026-05-23-m2-02b-mcp-client.md`).
//!
//! There used to be a separate `agent_scope.rs` holding an
//! `AgentScopedConnections` table (`AgentId -> server name -> connection
//! id`) for a subagent's own inline `mcpServers` (§24b). It and the
//! chain built on it were reverted, and the recorded reason was WRONG in
//! a way worth naming, because it sent the next reader down the same
//! dead end.
//!
//! What actually happened: that table presumed a subagent's connections
//! must live in the SHARED, name-keyed registry. Under that premise the
//! only way to stop two subagents colliding on one server name was to
//! mangle the name, which produced the FQN
//! `mcp____agent_scope__<uuid>__<server>__<tool>` — an empty server
//! segment that `tools/mcp::parse_full_name` rejects, `servers_with_tools`
//! drops, and no `mcp__<server>` permission rule can match.
//!
//! But the oracle never uses a shared table here. `Agr` (2.1.251
//! @~160977000) connects each frontmatter server under its PLAIN name and
//! keeps the clients in a per-spawn list that travels with the subagent;
//! `cleanup` tears down only the ones that spawn newly created, leaving a
//! parent's pre-existing connection alone. `PRn` (@160975900) is what
//! decides: a STRING spec references an existing disk-config server
//! (`isNewlyCreated:false`, never torn down), an OBJECT spec is an inline
//! definition (`isNewlyCreated:true`, torn down on exit). Two subagents
//! cannot collide because neither inline client is ever globally
//! registered.
//!
//! So the premise was the bug, not the naming. The port needs the table
//! key to be scoped internally while `config.name` stays plain — the
//! model-facing FQN, the permission rules and `oauth::server_key` all key
//! off that plain name and must not change.
//!
//! There used to be a separate `approval.rs` with its own
//! `McpApprovalPolicy`/`ApprovalStatus`; it was a dead duplicate (zero
//! external references, and its one config flag was written but never
//! read) of the approval gate that `server_gate.rs`'s `McpPolicyContext::
//! decide` actually implements, so it was deleted (§25b). If it is ever
//! revived: its `ConfigScope::Dynamic -> PendingApproval` mapping would
//! silently regress §27a, which made `--mcp-config` entries `Dynamic`
//! specifically so they are NOT approval-gated — see
//! `server_gate.rs`'s `decide` and its `project_approval_is_scope_aware`
//! test's `ConfigScope::Dynamic` case.

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 23 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]
// Dead code kept visible, not swept: this crate had 4 item(s) rustc could
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

pub mod capabilities;
pub mod client;
pub mod config_diagnostics;
pub mod connection;
pub mod discovery_cache;
pub mod enterprise_policy;
pub mod env_expansion;
pub mod headers_helper;
pub mod hook_dispatch;
pub mod identity;
pub mod inbound;
pub mod initialize_params;
pub mod json_config;
pub mod mcp_output_storage;
pub mod negotiation;
pub mod normalization;
pub mod oauth;
pub mod protocol_negotiation;
pub mod raw_conn;
pub mod registry;
pub mod server_gate;
pub mod tool_schema;
#[cfg(test)]
mod tracing_capture;
pub mod xaa;
pub mod xaa_idp;

pub use client::{
    truncate_description, McpClient, McpClientError, McpDirectoryEntry, MAX_MCP_DESCRIPTION_LENGTH,
    MAX_MCP_DIRECTORY_PAGES,
};
pub use connection::{
    ConfigScope, McpAgentSource, McpConnectionState, McpServerConfig, McpServerMetadata,
    McpServerRole,
};
pub use discovery_cache::{
    cache_gate, cache_gate_with_metadata, decide, decide_with_metadata,
    feature_enabled as discovery_cache_feature_enabled, max_stale_ms, miss_telemetry_value,
    strike_threshold, ttl_ms, CacheGateReason, Decision as DiscoveryCacheDecision,
    DiscoveryCacheEntry, DiscoveryCacheStore, EntryLookup as DiscoveryCacheEntryLookup, MissReason,
};
pub use env_expansion::{
    expand_env_vars_in_string, expand_with_env, startup_env_snapshot, EnvExpansion,
};
pub use hook_dispatch::{ElicitationHookOutcome, ElicitationHookRequest, HookDispatcher};
pub use identity::{
    ClientInfo, CLIENT_DESCRIPTION, CLIENT_INFO, CLIENT_NAME, CLIENT_TITLE, CLIENT_VERSION,
    MCP_WEBSITE_URL,
};
pub use inbound::{new_shared_roots, ElicitationCreateHandler, RootsListHandler, SharedRoots};
pub use initialize_params::{ClientCapabilities, InitializeParams};
pub use json_config::{
    build_server_from_json_entry, discovery_cache_flag, discovery_cache_is_schema_key_for,
    load_mcp_json_with_precedence, load_mcp_servers, parse_global_config_mcp_servers,
    parse_local_config_mcp_servers, parse_mcp_json_string, parse_plugin_mcp_json_string, role_flag,
    role_is_schema_key_for, server_entry_shape_is_valid, McpJsonError,
};
pub use mcp_output_storage::{
    binary_blob_saved_message, decode_base64, extension_for_mime_type, format_file_size,
    map_resource_contents, persist_binary_content, Base64Error, PersistBinaryResult,
    RawResourceContent as RawResourceContentRich,
};
pub use platform_api::{
    McpConnectOptions, McpConnectResult, McpNegotiatedProtocol, McpNotificationDto,
    McpNotificationStream, McpProtocolEra,
};
pub use raw_conn::RawConnectionProvider;
pub use registry::{
    ConversationExport, LocalAppExposure, ManagedLocalAppServer, McpCatalogChanged, McpCatalogKind,
    McpRegistry,
};
pub use server_gate::{
    apply_project_server_gate, is_builtin_computer_use, mcp_server_is_disabled, McpPolicyContext,
    McpServerBlockReason, McpServerDecision, BUILTIN_COMPUTER_USE_SERVER,
};
pub use xaa_idp::{MapServerOAuthLookup, ServerOAuthLookup, XaaIdpConfigProvider, XaaIdpSettings};
