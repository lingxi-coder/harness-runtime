//! MCP (Model Context Protocol) transport abstraction.
//!
//! The `McpTransport` trait is the boundary between the engine's MCP layer
//! (`lingxi-mcp`) and platform-specific transport implementations. Engine
//! code receives an `Arc<dyn McpTransport>` and never speaks the wire
//! protocol directly.
//!
//! See spec §7 (MCP) and D17 (Runtime boundary).

use async_trait::async_trait;
use futures_core::stream::Stream;
use futures_util::FutureExt;
use protocol::McpConnectionId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::pin::Pin;
use thiserror::Error;

/// Insertion-order-preserving header map for remote MCP transports.
///
/// claude-code's `getServerKey` hashes `JSON.stringify({type,url,headers})`
/// with the `headers` object keys in the config's insertion order (auth.ts:329,
/// slowOperations.ts:189 — plain `JSON.stringify`, which preserves a JS
/// object's key insertion order). A sorted map (e.g. `BTreeMap`/`HashMap`-then-
/// sort) would diverge for configs with 2+ headers in non-alphabetical order,
/// so the header order must be preserved end-to-end (config parse → spec →
/// `server_key`). [`indexmap::IndexMap`] serializes as a JSON object preserving
/// that order, so the on-disk/IPC shape is unchanged.
pub type McpHeaders = indexmap::IndexMap<String, String>;

/// Concrete transport configuration for one MCP server.
///
/// Mirrors the 8 transport variants described in spec §7.1 (the two
/// developer-IDE transports, `SseIde`/`WsIde`, added for oracle parity — see
/// mcp §10 in the byte-alignment doc). Platform
/// implementations decide which variants they support via
/// [`McpTransport::supported_transports`].
#[derive(Clone, Serialize, Deserialize)]
pub enum McpTransportSpec {
    /// Local subprocess speaking MCP over stdin/stdout. Desktop platforms only.
    Stdio {
        /// Executable to spawn.
        command: String,
        /// CLI arguments.
        args: Vec<String>,
        /// Extra environment variables.
        env: std::collections::HashMap<String, String>,
    },
    /// Server-Sent Events HTTP endpoint.
    Sse {
        /// Endpoint URL.
        url: String,
        /// Static request headers (insertion order preserved for `getServerKey`
        /// byte-parity — see [`McpHeaders`]).
        headers: McpHeaders,
        /// Optional executable that produces auth headers on demand.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        headers_helper: Option<String>,
        /// Optional OAuth 2.1 PKCE config.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oauth: Option<McpOAuthConfigDto>,
    },
    /// JSON-over-HTTP endpoint.
    Http {
        /// Endpoint URL.
        url: String,
        /// Static request headers (insertion order preserved for `getServerKey`
        /// byte-parity — see [`McpHeaders`]).
        headers: McpHeaders,
        /// Optional executable that produces auth headers on demand.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        headers_helper: Option<String>,
        /// Optional OAuth 2.1 PKCE config.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oauth: Option<McpOAuthConfigDto>,
    },
    /// Bidirectional WebSocket connection.
    WebSocket {
        /// Endpoint URL (`ws://` or `wss://`).
        url: String,
        /// Static request headers used at handshake.
        headers: McpHeaders,
        /// Optional executable that produces auth headers on demand.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        headers_helper: Option<String>,
    },
    /// Same-process registered server (typically a Rust-native plugin).
    InProcess {
        /// Lookup key in the in-process registry.
        registry_key: String,
    },
    /// SSE endpoint exposed by a developer IDE.
    SseIde {
        /// Endpoint URL.
        url: String,
        /// Human-readable IDE name (for logs and approval UI).
        ide_name: String,
        /// Optional local bearer token presented on the SSE GET and POST.
        ///
        /// Older plugin-config entries omit this field; local lockfile
        /// discovery supplies it so SSE and WebSocket IDE peers share the
        /// same authentication contract.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth_token: Option<String>,
        /// True when the IDE is hosted on Windows (affects path normalization).
        #[serde(default)]
        ide_running_in_windows: bool,
    },
    /// WebSocket endpoint exposed by a developer IDE.
    ///
    /// Oracle `d` (2.1.251 Mach-O @154584715): `f({type:N("ws-ide"),url:i(),
    /// ideName:i(),authToken:i().optional(),ideRunningInWindows:q().optional(),
    /// timeout:o().optional(),alwaysLoad:q().optional(),role:t()})` — the same
    /// shape as [`Self::SseIde`] plus an optional `authToken` used at the
    /// WebSocket handshake.
    WsIde {
        /// Endpoint URL (`ws://` or `wss://`).
        url: String,
        /// Human-readable IDE name (for logs and approval UI).
        ide_name: String,
        /// Optional bearer token presented at the WebSocket handshake.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        auth_token: Option<String>,
        /// True when the IDE is hosted on Windows (affects path normalization).
        #[serde(default)]
        ide_running_in_windows: bool,
    },
    /// Logical channel controlled by a host SDK / embedder.
    SdkControl {
        /// Identifier for the host-provided control channel.
        control_channel_id: String,
    },
}

/// Lightweight discriminator for [`McpTransportSpec`].
///
/// Used by platforms to declare which transports they can carry without
/// shipping a full spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub enum McpTransportKind {
    Stdio,
    Sse,
    Http,
    WebSocket,
    InProcess,
    SseIde,
    WsIde,
    SdkControl,
}

impl McpTransportSpec {
    /// Short transport-kind label for display (`"stdio"`, `"sse"`, `"http"`,
    /// `"websocket"`, `"inprocess"`, `"sse-ide"`, `"ws-ide"`, `"sdk-control"`).
    /// Used by `OrchestratorHandle::list_mcp_servers` (M6-07) to populate
    /// `McpServerInfo::transport`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Sse { .. } => "sse",
            Self::Http { .. } => "http",
            Self::WebSocket { .. } => "websocket",
            Self::InProcess { .. } => "inprocess",
            Self::SseIde { .. } => "sse-ide",
            Self::WsIde { .. } => "ws-ide",
            Self::SdkControl { .. } => "sdk-control",
        }
    }

    /// Transport-kind discriminator ([`McpTransportKind`]) for this spec. Used by
    /// the MCP client to pick the `GLd` idle-timeout default (stdio / remote /
    /// in-process) — see `mcp::client::mcp_tool_idle_timeout_for`.
    #[must_use]
    pub fn transport_kind(&self) -> McpTransportKind {
        match self {
            Self::Stdio { .. } => McpTransportKind::Stdio,
            Self::Sse { .. } => McpTransportKind::Sse,
            Self::Http { .. } => McpTransportKind::Http,
            Self::WebSocket { .. } => McpTransportKind::WebSocket,
            Self::InProcess { .. } => McpTransportKind::InProcess,
            Self::SseIde { .. } => McpTransportKind::SseIde,
            Self::WsIde { .. } => McpTransportKind::WsIde,
            Self::SdkControl { .. } => McpTransportKind::SdkControl,
        }
    }
}

impl fmt::Debug for McpTransportSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio { command, args, env } => formatter
                .debug_struct("Stdio")
                .field("command", command)
                .field("args", args)
                .field("env", env)
                .finish(),
            Self::Sse {
                url,
                headers,
                headers_helper,
                oauth,
            } => formatter
                .debug_struct("Sse")
                .field("url", url)
                .field("headers", headers)
                .field("headers_helper", headers_helper)
                .field("oauth", oauth)
                .finish(),
            Self::Http {
                url,
                headers,
                headers_helper,
                oauth,
            } => formatter
                .debug_struct("Http")
                .field("url", url)
                .field("headers", headers)
                .field("headers_helper", headers_helper)
                .field("oauth", oauth)
                .finish(),
            Self::WebSocket {
                url,
                headers,
                headers_helper,
            } => formatter
                .debug_struct("WebSocket")
                .field("url", url)
                .field("headers", headers)
                .field("headers_helper", headers_helper)
                .finish(),
            Self::InProcess { registry_key } => formatter
                .debug_struct("InProcess")
                .field("registry_key", registry_key)
                .finish(),
            Self::SseIde {
                url,
                ide_name,
                auth_token,
                ide_running_in_windows,
            } => formatter
                .debug_struct("SseIde")
                .field("url", url)
                .field("ide_name", ide_name)
                .field("auth_token", &redacted_auth_token(auth_token))
                .field("ide_running_in_windows", ide_running_in_windows)
                .finish(),
            Self::WsIde {
                url,
                ide_name,
                auth_token,
                ide_running_in_windows,
            } => formatter
                .debug_struct("WsIde")
                .field("url", url)
                .field("ide_name", ide_name)
                .field("auth_token", &redacted_auth_token(auth_token))
                .field("ide_running_in_windows", ide_running_in_windows)
                .finish(),
            Self::SdkControl { control_channel_id } => formatter
                .debug_struct("SdkControl")
                .field("control_channel_id", control_channel_id)
                .finish(),
        }
    }
}

fn redacted_auth_token(token: &Option<String>) -> Option<&'static str> {
    token.as_ref().map(|_| "<redacted>")
}

/// OAuth 2.1 PKCE configuration carried in [`McpTransportSpec`].
///
/// Full handshake state machine lives in `lingxi-mcp::oauth`; this DTO is
/// only the static config consumed by transport implementations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpOAuthConfigDto {
    /// OAuth client identifier registered with the auth server.
    #[serde(default, rename = "clientId", alias = "client_id")]
    pub client_id: Option<String>,
    /// Local port used for the loopback callback URL.
    #[serde(default, rename = "callbackPort", alias = "callback_port")]
    pub callback_port: Option<u16>,
    /// Discovery document URL for the authorization server.
    #[serde(
        default,
        rename = "authServerMetadataUrl",
        alias = "auth_server_metadata_url"
    )]
    pub auth_server_metadata_url: Option<String>,
    /// Space-delimited scopes pinned by configuration. Pinned scopes take
    /// precedence over discovery and are reused for a single auth retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<String>,
    /// Anthropic-account associate flag (spec §7.5).
    #[serde(default)]
    pub xaa: Option<bool>,
}

/// Handle returned by a successful [`McpTransport::connect`].
///
/// The engine threads this back into the trait for every subsequent
/// operation on the same logical connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpRawConnection {
    /// Stable connection identifier; used as a lookup key by the engine.
    pub connection_id: McpConnectionId,
}

/// Protocol revision family used by the MCP handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum McpProtocolEra {
    /// The established single-shot initialize flow.
    Legacy,
    /// The opt-in `server/discover` negotiation flow.
    Modern,
}

/// Result of protocol negotiation for a live MCP connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpNegotiatedProtocol {
    /// Family selected for this connection.
    pub era: McpProtocolEra,
    /// Exact MCP revision sent/accepted by the server.
    pub version: String,
}

/// Options passed to a transport's combined handshake operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpConnectOptions {
    /// Expected family. `None` lets the transport use its platform policy.
    pub expected_era: Option<McpProtocolEra>,
    /// Total connection/handshake deadline in milliseconds.
    pub deadline_ms: u64,
    /// Probe budget selected by the immutable negotiation decision. `None`
    /// means no modern probe is requested; transports must still clamp this
    /// value against the remaining total deadline when it is present.
    pub probe_timeout_ms: Option<u64>,
}

impl Default for McpConnectOptions {
    fn default() -> Self {
        Self {
            expected_era: None,
            deadline_ms: 100_000,
            probe_timeout_ms: None,
        }
    }
}

/// Combined connect + initialize outcome.  The default trait implementation
/// preserves the existing two-call behavior for every existing transport.
#[derive(Debug, Clone)]
pub struct McpConnectResult {
    /// Raw connection handle.
    pub connection: McpRawConnection,
    /// Capabilities returned by initialize.
    pub capabilities: ServerCapabilitiesDto,
    /// The protocol family and exact version used by the handshake.
    pub negotiated: McpNegotiatedProtocol,
}

struct ConnectCleanupGuard<'a, T: McpTransport + ?Sized> {
    transport: &'a T,
    connection_id: McpConnectionId,
    armed: bool,
}

impl<T: McpTransport + ?Sized> ConnectCleanupGuard<'_, T> {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl<T: McpTransport + ?Sized> Drop for ConnectCleanupGuard<'_, T> {
    fn drop(&mut self) {
        if self.armed {
            // This is deliberately synchronous: a cancelled handshake has no
            // executor future left in which to await `disconnect`.
            self.transport.disconnect_sync(self.connection_id);
        }
    }
}

/// Server capability flags returned by `initialize`.
///
/// Mirrors the JSON-RPC capability object — booleans indicate whether the
/// server exposes the corresponding feature category.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // mirrors the MCP wire spec exactly
pub struct ServerCapabilitiesDto {
    /// Server exposes one or more tools.
    pub tools: bool,
    /// Server exposes one or more resources.
    pub resources: bool,
    /// Server exposes one or more prompts.
    pub prompts: bool,
    /// Server emits log notifications.
    pub logging: bool,
    /// Server declares `resources/directory/read` support through the
    /// `io.modelcontextprotocol/skills.directoryRead` extension. Older
    /// serialized data or transports that omit this field deserialize to
    /// `false` via `#[serde(default)]`.
    #[serde(default)]
    pub directory_read: bool,
    /// Vendor-specific or experimental capability flags.
    pub experimental: std::collections::HashMap<String, Value>,
    /// Complete MCP extension map returned by `initialize`.
    ///
    /// `directory_read` remains as a derived compatibility convenience; this
    /// field preserves extension evidence needed by discovery-cache gates.
    #[serde(default)]
    pub extensions: std::collections::HashMap<String, Value>,
}

/// Per-tool permission policy declared by an MCP server/config producer.
/// Kept separate from [`McpToolDto`] so discovery-cache payloads and existing
/// DTO struct literals remain wire-compatible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpToolPermissionPolicy {
    /// Permit the tool without an additional prompt.
    AlwaysAllow,
    /// Require a prompt for every invocation.
    AlwaysAsk,
    /// Refuse every invocation.
    AlwaysDeny,
}

impl McpToolPermissionPolicy {
    /// Parse config spellings accepted by Claude Code's policy loader.
    #[must_use]
    pub fn from_policy_str(value: &str) -> Option<Self> {
        match value {
            "allow" | "always_allow" => Some(Self::AlwaysAllow),
            "ask" | "always_ask" => Some(Self::AlwaysAsk),
            "deny" | "always_deny" => Some(Self::AlwaysDeny),
            _ => None,
        }
    }

    /// Return the strictest of two declarations (`deny` > `ask` > `allow`).
    #[must_use]
    pub const fn strictest(self, other: Self) -> Self {
        match (self, other) {
            (Self::AlwaysDeny, _) | (_, Self::AlwaysDeny) => Self::AlwaysDeny,
            (Self::AlwaysAsk, _) | (_, Self::AlwaysAsk) => Self::AlwaysAsk,
            (Self::AlwaysAllow, Self::AlwaysAllow) => Self::AlwaysAllow,
        }
    }
}

/// Tighten-only per-tool ceiling supplied by host/org policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpPermissionCeiling {
    /// No additional restriction.
    Allow,
    /// Require interactive approval at minimum.
    Ask,
    /// Block the tool.
    Deny,
}

impl McpPermissionCeiling {
    /// Parse `toolPermissions`/org-policy values (`blocked` maps to `deny`).
    #[must_use]
    pub fn from_policy_str(value: &str) -> Option<Self> {
        match value {
            "allow" => Some(Self::Allow),
            "ask" => Some(Self::Ask),
            "blocked" | "deny" => Some(Self::Deny),
            _ => None,
        }
    }

    /// Return the stricter of two ceilings (`deny` > `ask` > `allow`).
    #[must_use]
    pub const fn strictest(self, other: Self) -> Self {
        match (self, other) {
            (Self::Deny, _) | (_, Self::Deny) => Self::Deny,
            (Self::Ask, _) | (_, Self::Ask) => Self::Ask,
            (Self::Allow, Self::Allow) => Self::Allow,
        }
    }
}

/// Standard MCP icon descriptor for a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpIconDto {
    /// Verified icon source URI.
    pub src: String,
    /// Optional MIME type for the icon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Declared size labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sizes: Vec<String>,
    /// Optional theme discriminator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Unrecognized extension members preserved across transport/cache hops.
    #[serde(flatten, default, skip_serializing_if = "indexmap::IndexMap::is_empty")]
    pub extra: indexmap::IndexMap<String, Value>,
}

/// Optional MCP tool annotations.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolAnnotationsDto {
    /// Optional user-facing title override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Whether the tool is host-proven read-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    /// Whether the tool may be destructive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    /// Whether the host can prove the tool is idempotent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    /// Whether the tool touches the outside world.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
    /// Unrecognized annotation hints preserved across transport/cache hops.
    #[serde(flatten, default, skip_serializing_if = "indexmap::IndexMap::is_empty")]
    pub extra: indexmap::IndexMap<String, Value>,
}

/// MCP task-support declaration for one tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpToolTaskSupportDto {
    /// This tool does not participate in MCP Tasks.
    Forbidden,
    /// This tool may opt into MCP Tasks.
    Optional,
    /// This tool requires MCP Tasks.
    Required,
}

/// Optional execution metadata for one tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolExecutionDto {
    /// Declared MCP Tasks support level for this tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_support: Option<McpToolTaskSupportDto>,
}

/// Standard MCP 2025-11-25 `Tool` wire definition used by Local App catalogs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolDefinitionDto {
    /// Raw tool name exposed by the server.
    pub name: String,
    /// Optional user-facing title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional user-facing description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Input JSON Schema.
    pub input_schema: Value,
    /// Optional structured output JSON Schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// Optional derived annotations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<McpToolAnnotationsDto>,
    /// Optional execution metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<McpToolExecutionDto>,
    /// Optional icon list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub icons: Vec<McpIconDto>,
    /// Opaque vendor metadata preserved byte-for-byte.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

impl McpToolDefinitionDto {
    /// Construct a minimal wire `Tool` definition.
    #[must_use]
    pub fn new(name: impl Into<String>, input_schema: Value) -> Self {
        Self {
            name: name.into(),
            title: None,
            description: None,
            input_schema,
            output_schema: None,
            annotations: None,
            execution: None,
            icons: Vec::new(),
            meta: None,
        }
    }
}

/// Config-side policy record used to seed the MCP client's permission map.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct McpConfiguredToolPolicyDto {
    /// Raw upstream tool name before MCP FQN qualification.
    pub name: String,
    /// Optional server-declared policy.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "permissionPolicy"
    )]
    pub permission_policy: Option<McpToolPermissionPolicy>,
    /// Optional host/org ceiling for this tool.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "orgMaxPermission"
    )]
    pub org_max_permission: Option<McpPermissionCeiling>,
}

/// One tool advertised by an MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpToolDto {
    /// Server name (logical, e.g. registry key).
    pub server_name: String,
    /// Tool name as exposed by the server.
    pub tool_name: String,
    /// Human-readable description shown to the model.
    pub description: String,
    /// JSON-Schema for the tool's input.
    pub input_schema: Value,
    /// Optional JSON-Schema for the tool's structured output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    /// Optional standardized tool annotations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<McpToolAnnotationsDto>,
    /// Optional icon list preserved from the upstream definition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub icons: Vec<McpIconDto>,
    /// Opaque vendor metadata preserved byte-for-byte.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
    /// Engine-side fully-qualified identifier (`mcp__<server>__<tool>`).
    pub full_name: String,
    /// Retrieval prefilter hint from `tool._meta['anthropic/searchHint']`
    /// (e.g. `"shell"`, `"editor"`). `None` when absent or when the server
    /// did not set `_meta`. Forwarded from [`mcp::client::ToolMeta`] per
    /// `client.ts:1777-1778`. Used by tool-search ranking to prefer tools
    /// whose `searchHint` matches the current task description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_hint: Option<String>,
    /// Force-include flag from `tool._meta['anthropic/alwaysLoad']`. When
    /// `true`, the tool is included in the agent prompt regardless of whether
    /// the `searchHint` matches. Forwarded from `client.ts:1779-1780`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub always_load: Option<bool>,
    /// `true` when `tool._meta['anthropic/requiresUserInteraction'] === true`
    /// (client.ts factory, binary-confirmed @182519150). Marks a tool that
    /// needs fresh, in-the-moment user interaction on every call (e.g. an
    /// embedded OAuth/consent step) — a stored "always allow" rule cannot
    /// satisfy that, so a persistent grant must never be offered/written for
    /// it (oracle `suppressesAlwaysAllowRule` @182520462). Forwarded onto
    /// `MCPTool`'s `Tool::requires_user_interaction` override; consumed by
    /// the TUI permission dialog to hide "Yes, allow always"
    /// (`tui/src/permission_gate.rs`, `tui/src/bottom_pane/permission_view.rs`).
    /// Defaults to `false` for servers/paths that don't set it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub requires_user_interaction: bool,
}

impl McpToolDto {
    /// Raw upstream tool name.
    #[must_use]
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// Model-facing tool description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Input JSON Schema advertised for the tool.
    #[must_use]
    pub const fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    /// Whether every invocation requires fresh user interaction.
    #[must_use]
    pub const fn requires_user_interaction(&self) -> bool {
        self.requires_user_interaction
    }
}

/// `serde(skip_serializing_if)` helper for a plain `bool` field defaulting to
/// `false` — keeps the common (unset) case terse in serialized form.
fn is_false(b: &bool) -> bool {
    !b
}

/// One resource advertised by an MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpResourceDto {
    /// Resource URI.
    pub uri: String,
    /// Human-readable name.
    pub name: String,
    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional content type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Opaque vendor metadata preserved byte-for-byte.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// One parameterized resource template advertised by an MCP server
/// (`resources/templates/list`).
///
/// Distinct from [`McpResourceDto`], which carries a concrete `uri`: a
/// template's `uri_template` is an RFC 6570 URI Template with `{variable}`
/// placeholders a client fills in before issuing `resources/read` against the
/// resolved URI (MCP spec `ListResourceTemplatesResult` /
/// `ResourceTemplate`). Oracle `GGt = f({...DYe.shape,...dEt.shape,
/// uriTemplate:i(),description:qT(i()),mimeType:qT(i()),
/// annotations:LYe.optional(),_meta:qT(un({}))})` (2.1.251 Mach-O
/// @167622755); the response envelope is `{resourceTemplates:[...]}` (oracle
/// `MYe = yEt.extend({resourceTemplates:H(GGt)})`, same offset).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpResourceTemplateDto {
    /// RFC 6570 URI template, e.g. `"file:///{path}"`.
    #[serde(rename = "uriTemplate")]
    pub uri_template: String,
    /// Human-readable name.
    pub name: String,
    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional content type.
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Optional resource annotations preserved as arbitrary JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
    /// Opaque vendor metadata preserved from the wire.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// One prompt advertised by an MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpPromptDto {
    /// Prompt name as exposed by the server.
    pub name: String,
    /// Optional human-readable description.
    pub description: Option<String>,
    /// Named arguments accepted by `prompts/get`, in server-declared order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<McpPromptArgumentDto>,
}

/// One named argument declared by an MCP prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpPromptArgumentDto {
    /// Wire argument name sent to `prompts/get`.
    pub name: String,
    /// Optional human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the server requires this argument.
    #[serde(default)]
    pub required: bool,
}

/// Result of a tool invocation.
///
/// The MCP `CallToolResult` wire shape also carries two optional, arbitrary
/// JSON members alongside `content`/`isError`: `_meta` and `structuredContent`
/// (MCP spec; claude-code reads both off the raw result in
/// `services/mcp/client.ts` and forwards them onto the surfaced `ToolResult`).
/// Both are carried here verbatim (no transformation) and default to `None`
/// when the server omits them, so non-claude-code servers decode cleanly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpToolResultDto {
    /// JSON content returned by the tool.
    pub content: Value,
    /// True when the server flagged the result as an error.
    pub is_error: bool,
    /// Server-supplied `_meta` block (arbitrary JSON object), passed through
    /// byte-for-byte. `None` when the wire result omits `_meta`.
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none", default)]
    pub meta: Option<Value>,
    /// Server-supplied `structuredContent` (arbitrary JSON), passed through
    /// byte-for-byte. `None` when the wire result omits `structuredContent`.
    #[serde(
        rename = "structuredContent",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub structured_content: Option<Value>,
}

/// Arbitrary notification pushed by a server (logging, progress, etc).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpNotificationDto {
    /// JSON-RPC method.
    pub method: String,
    /// Method-specific parameters.
    pub params: Value,
}

/// Server-initiated elicitation request (asks the host to gather user input).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitRequestDto {
    /// Server-supplied request parameters.
    pub params: Value,
}

/// Reply to an [`ElicitRequestDto`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitResultDto {
    /// User-supplied data being returned to the server.
    pub data: Value,
}

/// Contents read from an MCP resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResourceContentDto {
    /// Echoed URI.
    pub uri: String,
    /// Resource body (text-encoded; binaries are base64).
    pub content: String,
    /// Optional content type for the returned body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Opaque vendor metadata preserved byte-for-byte.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Value>,
}

/// One element of a `resources/read` `contents[]` array, preserving the full
/// claude-code shape (`ReadMcpResourceTool.ts:106-139`).
///
/// Unlike the legacy [`McpResourceContentDto`] (which collapses a read to a
/// single `{uri, content}` and drops the `mimeType`/blob distinction), this
/// DTO carries every field the TS tool surfaces:
///
/// * a text content block → `text` is `Some`, `blob_saved_to` is `None`;
/// * a base64 *blob* content block → the bytes are decoded and persisted to
///   disk, `blob_saved_to` holds the path, and `text` carries the
///   `getBinaryBlobSavedMessage` line (or a "could not be saved" message on a
///   persistence failure);
/// * an empty/unrecognized block → both `text` and `blob_saved_to` are `None`.
///
/// `mime_type` is carried verbatim from the wire (absent → `None`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpResourceContentsRich {
    /// Echoed resource URI for this content block.
    pub uri: String,
    /// MIME type as advertised by the server (absent on the wire → `None`).
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none", default)]
    pub mime_type: Option<String>,
    /// Opaque vendor metadata preserved byte-for-byte.
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none", default)]
    pub meta: Option<Value>,
    /// Text content of the block (text blocks; also the human-readable
    /// "saved to disk" message for persisted blobs).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub text: Option<String>,
    /// Filesystem path a decoded binary blob was persisted to, if any.
    #[serde(
        rename = "blobSavedTo",
        skip_serializing_if = "Option::is_none",
        default
    )]
    pub blob_saved_to: Option<String>,
}

/// Stream of notifications pushed by a server.
pub type McpNotificationStream = Pin<Box<dyn Stream<Item = McpNotificationDto> + Send>>;

/// Transport boundary between the engine's MCP layer and the wire protocol.
///
/// Implementations live in platform crates. The engine never speaks JSON-RPC
/// directly — it builds an [`McpTransportSpec`], calls [`Self::connect`], and
/// then works with the returned [`McpRawConnection`] handle.
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// Open a logical connection to the server described by `spec`.
    async fn connect(&self, spec: &McpTransportSpec) -> Result<McpRawConnection, McpError>;

    /// Open and initialize a connection in one operation.  Existing
    /// transports retain the legacy behavior through this default; remote
    /// transports may override it to perform an opt-in modern probe.
    async fn connect_and_initialize(
        &self,
        spec: &McpTransportSpec,
        options: McpConnectOptions,
    ) -> Result<McpConnectResult, McpError> {
        let deadline = tokio::time::Instant::now()
            .checked_add(std::time::Duration::from_millis(options.deadline_ms))
            .ok_or_else(|| McpError::Connection("MCP connection deadline overflow".into()))?;
        let connection = tokio::time::timeout_at(deadline, self.connect(spec))
            .await
            .map_err(|_| McpError::Connection("MCP connection deadline exceeded".into()))??;
        let cleanup = ConnectCleanupGuard {
            transport: self,
            connection_id: connection.connection_id,
            armed: true,
        };
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or_default();
        let capabilities = match std::panic::AssertUnwindSafe(tokio::time::timeout(
            remaining,
            self.initialize(&connection),
        ))
        .catch_unwind()
        .await
        {
            Ok(Ok(Ok(capabilities))) => capabilities,
            Ok(Ok(Err(error))) => {
                if self.disconnect(connection.connection_id).await.is_ok() {
                    cleanup.disarm();
                }
                return Err(error);
            }
            Ok(Err(_)) => {
                if self.disconnect(connection.connection_id).await.is_ok() {
                    cleanup.disarm();
                }
                return Err(McpError::Connection(
                    "MCP connection deadline exceeded".into(),
                ));
            }
            Err(payload) => {
                if self.disconnect(connection.connection_id).await.is_ok() {
                    cleanup.disarm();
                }
                std::panic::resume_unwind(payload);
            }
        };
        cleanup.disarm();
        Ok(McpConnectResult {
            connection,
            capabilities,
            negotiated: McpNegotiatedProtocol {
                // The default implementation is the legacy two-call flow;
                // an implementation must explicitly override this method
                // before it may claim a modern negotiation.
                era: McpProtocolEra::Legacy,
                version: "2025-11-25".to_string(),
            },
        })
    }

    /// Perform the MCP `initialize` handshake and return the server's
    /// declared capabilities.
    async fn initialize(&self, conn: &McpRawConnection) -> Result<ServerCapabilitiesDto, McpError>;

    /// Enumerate all tools exposed by the server.
    async fn list_tools(&self, conn: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError>;

    /// Enumerate all resources exposed by the server.
    async fn list_resources(
        &self,
        conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceDto>, McpError>;

    /// Enumerate parameterized resource templates exposed by the server
    /// (`resources/templates/list` — see [`McpResourceTemplateDto`]).
    ///
    /// Additive method: the DEFAULT body returns an empty list so every
    /// existing implementation compiles unchanged, exactly like
    /// [`Self::read_resource_rich`]'s default below. Production transports
    /// (POSIX) override it to issue the real wire call; a server (or a stub
    /// transport) that never declares templates is unaffected.
    async fn list_resource_templates(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceTemplateDto>, McpError> {
        Ok(Vec::new())
    }

    /// Enumerate all prompts exposed by the server.
    async fn list_prompts(&self, conn: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError>;

    /// Invoke a tool by name with JSON `input`.
    async fn call_tool(
        &self,
        conn: &McpRawConnection,
        tool: &str,
        input: Value,
    ) -> Result<McpToolResultDto, McpError>;

    /// Read the contents of a resource by URI.
    async fn read_resource(
        &self,
        conn: &McpRawConnection,
        uri: &str,
    ) -> Result<McpResourceContentDto, McpError>;

    /// Read the FULL multi-content `contents[]` array of a resource, preserving
    /// `mimeType`, distinguishing text from base64 blobs, and persisting
    /// decoded blobs to disk (`blob_saved_to`). Mirrors claude-code's
    /// `ReadMcpResourceTool.ts:106-139`.
    ///
    /// `output_dir` is the directory binary blobs are written to (the
    /// `getToolResultsDir()` equivalent); the caller picks a session/temp dir.
    ///
    /// Additive method: the DEFAULT body wraps [`Self::read_resource`] so every
    /// existing implementation compiles unchanged — it returns a single
    /// text-only entry from the legacy single-content DTO and never persists a
    /// blob. Production transports (POSIX) override it to return the genuine
    /// multi-content array with blob persistence.
    async fn read_resource_rich(
        &self,
        conn: &McpRawConnection,
        uri: &str,
        _output_dir: &std::path::Path,
    ) -> Result<Vec<McpResourceContentsRich>, McpError> {
        let single = self.read_resource(conn, uri).await?;
        Ok(vec![McpResourceContentsRich {
            uri: single.uri,
            mime_type: single.mime_type,
            meta: single.meta,
            text: Some(single.content),
            blob_saved_to: None,
        }])
    }

    /// Liveness probe used by the registry's health checker.
    async fn ping(&self, conn_id: McpConnectionId) -> Result<(), McpError>;

    /// Stream of server-pushed notifications. Implementations should
    /// multiplex multiple subscribers if necessary.
    async fn notifications(
        &self,
        conn: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError>;

    /// Handle one server-initiated elicitation request and return the reply.
    async fn handle_elicitation(
        &self,
        conn: &McpRawConnection,
        req: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError>;

    /// Tear down the connection and release any resources.
    async fn disconnect(&self, conn_id: McpConnectionId) -> Result<(), McpError>;

    /// Synchronous best-effort teardown hook used when a combined handshake
    /// future is cancelled. Implementations owning resources that cannot be
    /// dropped safely should override this alongside [`Self::disconnect`].
    fn disconnect_sync(&self, _conn_id: McpConnectionId) {}

    /// Transports this implementation can carry on the current platform.
    fn supported_transports(&self) -> Vec<McpTransportKind>;
}

#[cfg(test)]
mod tests {
    use super::{
        McpIconDto, McpResourceContentDto, McpResourceContentsRich, McpResourceDto,
        McpResourceTemplateDto, McpToolAnnotationsDto, McpToolDto, McpTransportSpec,
    };
    use serde_json::json;

    #[test]
    fn mcp_tool_dto_preserves_optional_metadata_fields() {
        let value = serde_json::to_value(McpToolDto {
            server_name: "local_app_habits".to_string(),
            tool_name: "save_habit".to_string(),
            description: "Save one habit entry.".to_string(),
            input_schema: json!({"type":"object"}),
            output_schema: Some(json!({"type":"object","properties":{"ok":{"type":"boolean"}}})),
            annotations: Some(McpToolAnnotationsDto {
                title: Some("Save habit".to_string()),
                read_only_hint: Some(false),
                destructive_hint: Some(false),
                idempotent_hint: Some(true),
                open_world_hint: Some(false),
                extra: indexmap::IndexMap::from([(
                    "vendor/riskTier".to_string(),
                    json!("reviewed"),
                )]),
            }),
            icons: vec![McpIconDto {
                src: "https://example.invalid/icon.svg".to_string(),
                mime_type: Some("image/svg+xml".to_string()),
                sizes: vec!["64x64".to_string()],
                theme: None,
                extra: indexmap::IndexMap::from([("vendor/accent".to_string(), json!("blue"))]),
            }],
            meta: Some(json!({"openai/outputTemplate":"ui://local-app/habits/widget.html"})),
            full_name: "mcp__local_app_habits__save_habit".to_string(),
            search_hint: Some("habit tracker".to_string()),
            always_load: Some(true),
            requires_user_interaction: false,
        })
        .expect("serialize tool dto");

        assert_eq!(value["output_schema"]["type"], "object");
        assert_eq!(value["annotations"]["title"], "Save habit");
        assert_eq!(value["annotations"]["vendor/riskTier"], "reviewed");
        assert_eq!(value["icons"][0]["src"], "https://example.invalid/icon.svg");
        assert_eq!(value["icons"][0]["vendor/accent"], "blue");
        assert_eq!(
            value["_meta"]["openai/outputTemplate"],
            "ui://local-app/habits/widget.html"
        );
    }

    #[test]
    fn mcp_resource_dtos_preserve_description_mime_and_meta() {
        let resource = serde_json::to_value(McpResourceDto {
            uri: "ui://local-app/habits/widget.html".to_string(),
            name: "Habits widget".to_string(),
            description: Some("Interactive MCP App widget".to_string()),
            mime_type: Some("text/html;profile=mcp-app".to_string()),
            meta: Some(json!({"openai/widgetPrefersBorder":true})),
        })
        .expect("serialize resource dto");
        assert_eq!(resource["description"], "Interactive MCP App widget");
        assert_eq!(resource["mime_type"], "text/html;profile=mcp-app");
        assert_eq!(resource["_meta"]["openai/widgetPrefersBorder"], true);

        let template = serde_json::to_value(McpResourceTemplateDto {
            uri_template: "file:///{path}".to_string(),
            name: "Workspace file".to_string(),
            description: Some("One workspace file".to_string()),
            mime_type: Some("text/plain".to_string()),
            annotations: Some(json!({"audience":["assistant"],"vendor/rank":7})),
            meta: Some(json!({"vendor/template":"opaque"})),
        })
        .expect("serialize resource template dto");
        assert_eq!(template["annotations"]["vendor/rank"], 7);
        assert_eq!(template["_meta"]["vendor/template"], "opaque");

        let content = serde_json::to_value(McpResourceContentDto {
            uri: "ui://local-app/habits/widget.html".to_string(),
            content: "<!doctype html>".to_string(),
            mime_type: Some("text/html;profile=mcp-app".to_string()),
            meta: Some(json!({"openai/widgetDescription":"Habits widget"})),
        })
        .expect("serialize resource content dto");
        assert_eq!(content["mime_type"], "text/html;profile=mcp-app");
        assert_eq!(
            content["_meta"]["openai/widgetDescription"],
            "Habits widget"
        );

        let rich = serde_json::to_value(McpResourceContentsRich {
            uri: "ui://local-app/habits/widget.html".to_string(),
            mime_type: Some("text/html;profile=mcp-app".to_string()),
            meta: Some(json!({"openai/widgetCSP":{"connect_domains":[]}})),
            text: Some("<!doctype html>".to_string()),
            blob_saved_to: None,
        })
        .expect("serialize rich resource dto");
        assert_eq!(rich["mimeType"], "text/html;profile=mcp-app");
        assert!(rich.get("blobSavedTo").is_none());
        assert_eq!(
            rich["_meta"]["openai/widgetCSP"]["connect_domains"],
            json!([])
        );
    }

    #[test]
    fn ide_transport_debug_redacts_local_auth_tokens() {
        let token = "local-only-token";
        let specs = [
            McpTransportSpec::SseIde {
                url: "http://127.0.0.1:43123/sse".into(),
                ide_name: "VS Code".into(),
                auth_token: Some(token.into()),
                ide_running_in_windows: false,
            },
            McpTransportSpec::WsIde {
                url: "ws://127.0.0.1:43124".into(),
                ide_name: "Cursor".into(),
                auth_token: Some(token.into()),
                ide_running_in_windows: false,
            },
        ];
        for spec in specs {
            let rendered = format!("{spec:?}");
            assert!(!rendered.contains(token));
            assert!(rendered.contains("<redacted>"));
        }
    }
}

/// Failure modes shared by every [`McpTransport`] method.
#[derive(Debug, Clone, Error)]
pub enum McpError {
    /// Transport variant is not implemented on the current platform.
    #[error("transport {0:?} not supported on this platform")]
    UnsupportedTransport(McpTransportKind),
    /// Underlying transport (TCP / stdio / etc.) failed.
    #[error("connection failed: {0}")]
    Connection(String),
    /// MCP `initialize` handshake failed or returned an invalid response.
    #[error("handshake failed: {0}")]
    Handshake(String),
    /// Remote HTTP response metadata retained for authentication recovery.
    #[error(
        "HTTP {status}{detail}",
        detail = www_authenticate
            .as_ref()
            .map(|value| format!(": {value}"))
            .unwrap_or_default()
    )]
    HttpResponse {
        /// HTTP response status.
        status: u16,
        /// `WWW-Authenticate` response header, when supplied by the server.
        www_authenticate: Option<String>,
    },
    /// OAuth handshake aborted or returned an error.
    #[error("oauth flow failed: {0}")]
    OAuth(String),
    /// Server does not advertise the requested tool.
    #[error("tool not found: {0}")]
    ToolNotFound(String),
    /// Tool invocation exceeded the per-call timeout budget.
    ///
    /// Display format is **load-bearing** — integration tests (plan M2-07) and
    /// REPL surface match against the exact string. Lock changes via plan
    /// `2026-05-23-m2-02b-mcp-client.md`.
    #[error("MCP server \"{server}\" tool \"{tool}\" timed out after {secs}s")]
    Timeout {
        /// Logical server name (registry key).
        server: String,
        /// Tool name (unprefixed — no `mcp__` prefix).
        tool: String,
        /// Timeout budget in whole seconds.
        secs: u64,
    },
    /// Catch-all for unexpected failures.
    #[error("internal error: {0}")]
    Internal(String),
}
