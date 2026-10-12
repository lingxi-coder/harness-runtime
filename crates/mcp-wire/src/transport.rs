//! MCP (Model Context Protocol) transport abstraction.
//!
//! The `McpTransport` trait is the boundary between the MCP client layer and
//! platform-specific transport implementations. Engine
//! code receives an `Arc<dyn McpTransport>` and never speaks the wire
//! protocol directly.
//!
//! See spec §7 (MCP) and D17 (Runtime boundary).

use crate::McpConnectionId;
use async_trait::async_trait;
use futures_core::stream::Stream;
use futures_util::FutureExt;
use json_projection::Utf16JsonProjection;
use serde::{Deserialize, Serialize};
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

/// Server-owned metadata retained from a completed MCP handshake.
/// This is distinct from the host's configured server identity and permissions.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerMetadataDto {
    /// Complete declared capabilities, including protocol-specific nested data.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "json_projection::raw_json_serde::serialize_option",
        deserialize_with = "json_projection::raw_json_serde::deserialize_option"
    )]
    pub raw_capabilities: Option<json_projection::Utf16JsonProjection>,
    /// Implementation information advertised by the server.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "json_projection::raw_json_serde::serialize_option",
        deserialize_with = "json_projection::raw_json_serde::deserialize_option"
    )]
    pub server_info: Option<json_projection::Utf16JsonProjection>,
    /// Optional server instructions supplied to the model.
    #[serde(
        default,
        serialize_with = "serialize_optional_mcp_string",
        deserialize_with = "deserialize_optional_mcp_string"
    )]
    pub instructions: Option<json_projection::Utf16JsonProjection>,
    /// Complete modern discovery result, when negotiation used `server/discover`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "json_projection::raw_json_serde::serialize_option",
        deserialize_with = "json_projection::raw_json_serde::deserialize_option"
    )]
    pub discovery: Option<json_projection::Utf16JsonProjection>,
}

/// Current elicitation capability wire shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpElicitationMode {
    /// Current explicit bare capability; the MCP schema permits form requests.
    Bare,
    /// Both current form and URL elicitation requests are supported.
    FormAndUrl,
}

impl McpElicitationMode {
    /// Exact capability object used on the current MCP wire.
    #[must_use]
    pub fn wire(self) -> json_projection::Utf16JsonProjection {
        match self {
            Self::Bare => json_projection::Utf16JsonProjection::parse("{}")
                .expect("empty elicitation capability is valid JSON"),
            Self::FormAndUrl => {
                json_projection::Utf16JsonProjection::parse(r#"{"form":{},"url":{}}"#)
                    .expect("generated elicitation capability is valid JSON")
            }
        }
    }
}

/// Frozen capability choices for both current protocol families on one connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct McpElicitationCapabilities {
    /// Initialize uses per-server eligibility and the explicit bare config.
    pub legacy: McpElicitationMode,
    /// Modern discovery and request metadata use the current URL feature gate.
    pub modern: McpElicitationMode,
}

impl Default for McpElicitationCapabilities {
    fn default() -> Self {
        Self {
            legacy: McpElicitationMode::FormAndUrl,
            modern: McpElicitationMode::FormAndUrl,
        }
    }
}

impl McpElicitationCapabilities {
    /// Default for direct transport calls without an engine config producer.
    #[must_use]
    pub fn for_transport(kind: McpTransportKind) -> Self {
        let mut capabilities = Self::default();
        if !matches!(
            kind,
            McpTransportKind::Stdio
                | McpTransportKind::Sse
                | McpTransportKind::Http
                | McpTransportKind::WebSocket
        ) {
            capabilities.legacy = McpElicitationMode::Bare;
        }
        capabilities
    }
}

/// Closed capability projection; does not advertise absent host task or sampling producers.
#[must_use]
pub fn mcp_client_capabilities(
    elicitation: McpElicitationMode,
) -> json_projection::Utf16JsonProjection {
    let source = json_projection::Utf16JsonProjection::parse(
        r#"{"roots":{"listChanged":true},"elicitation":{}}"#,
    )
    .expect("generated client capabilities are valid JSON");
    let mut capabilities = source;
    capabilities
        .replace_pointer_projection("/elicitation", elicitation.wire())
        .expect("capabilities contain the generated elicitation object");
    capabilities
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
    /// Capability authority frozen by the current config/host at connect ingress.
    pub elicitation: McpElicitationCapabilities,
}

impl Default for McpConnectOptions {
    fn default() -> Self {
        Self {
            expected_era: None,
            deadline_ms: 100_000,
            probe_timeout_ms: None,
            elicitation: McpElicitationCapabilities::default(),
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
fn empty_source_json_object() -> json_projection::Utf16JsonProjection {
    json_projection::Utf16JsonProjection::parse("{}").expect("empty source JSON object is valid")
}

fn is_empty_slice<T>(value: &&[T]) -> bool {
    value.is_empty()
}

fn deserialize_optional_mcp_string<'de, D>(
    deserializer: D,
) -> Result<Option<json_projection::Utf16JsonProjection>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let projection = json_projection::raw_json_serde::deserialize(deserializer)?;
    let root = projection
        .at_pointer("")
        .expect("a parsed projection always has a root");
    if root.is_null() {
        Ok(None)
    } else if root.code_units().is_some() {
        Ok(Some(projection))
    } else {
        Err(serde::de::Error::custom("expected a string or null"))
    }
}

fn serialize_optional_mcp_string<S>(
    value: &Option<json_projection::Utf16JsonProjection>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if let Some(projection) = value {
        if projection.string_units("").is_none() {
            return Err(serde::ser::Error::custom(
                "MCP text value must be a root-string projection",
            ));
        }
    }
    json_projection::raw_json_serde::serialize_option(value, serializer)
}

fn deserialize_required_mcp_string<'de, D>(
    deserializer: D,
) -> Result<json_projection::Utf16JsonProjection, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let projection = json_projection::raw_json_serde::deserialize(deserializer)?;
    if projection.string_units("").is_some() {
        Ok(projection)
    } else {
        Err(serde::de::Error::custom(
            "MCP text value must be a JSON string",
        ))
    }
}

fn serialize_required_mcp_string<S>(
    value: &json_projection::Utf16JsonProjection,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if value.string_units("").is_none() {
        return Err(serde::ser::Error::custom(
            "MCP text value must be a root-string projection",
        ));
    }
    json_projection::raw_json_serde::serialize(value, serializer)
}

fn source_object_without_fields(
    source: &json_projection::Utf16JsonProjection,
    known_fields: &[&str],
) -> Result<json_projection::Utf16JsonProjection, json_projection::Utf16JsonProjectionError> {
    let entries = source
        .object_entries()
        .ok_or(json_projection::Utf16JsonProjectionError::ExpectedObject)?;
    let mut fields = Vec::with_capacity(entries.len());
    for entry in entries {
        if known_fields
            .iter()
            .any(|known| known.encode_utf16().eq(entry.code_units.iter().copied()))
        {
            continue;
        }
        let value = source.subprojection_property_units(entry.code_units)?;
        fields.push((entry.code_units.to_vec(), value));
    }
    json_projection::Utf16JsonProjection::object_units(fields)
}

fn merge_source_object_fields(
    known: &json_projection::Utf16JsonProjection,
    extra: &json_projection::Utf16JsonProjection,
    known_fields: &[&str],
) -> Result<json_projection::Utf16JsonProjection, json_projection::Utf16JsonProjectionError> {
    let known_entries = known
        .object_entries()
        .ok_or(json_projection::Utf16JsonProjectionError::ExpectedObject)?;
    let extra_entries = extra
        .object_entries()
        .ok_or(json_projection::Utf16JsonProjectionError::ExpectedObject)?;
    let mut fields = Vec::with_capacity(known_entries.len() + extra_entries.len());
    for entry in known_entries {
        fields.push((
            entry.code_units.to_vec(),
            known.subprojection_property_units(entry.code_units)?,
        ));
    }
    for entry in extra_entries {
        if known_fields
            .iter()
            .any(|known| known.encode_utf16().eq(entry.code_units.iter().copied()))
        {
            continue;
        }
        fields.push((
            entry.code_units.to_vec(),
            extra.subprojection_property_units(entry.code_units)?,
        ));
    }
    json_projection::Utf16JsonProjection::object_units(fields)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // mirrors the MCP wire spec exactly
/// Negotiated server capability flags and exact extension payloads.
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
    #[serde(
        default = "empty_source_json_object",
        with = "json_projection::raw_json_serde"
    )]
    pub experimental: json_projection::Utf16JsonProjection,
    /// Complete MCP extension map returned by `initialize`.
    ///
    /// `directory_read` remains as a derived compatibility convenience; this
    /// field preserves extension evidence needed by discovery-cache gates.
    #[serde(
        default = "empty_source_json_object",
        with = "json_projection::raw_json_serde"
    )]
    pub extensions: json_projection::Utf16JsonProjection,
}

impl Default for ServerCapabilitiesDto {
    fn default() -> Self {
        let empty = empty_source_json_object();
        Self {
            tools: false,
            resources: false,
            prompts: false,
            logging: false,
            directory_read: false,
            experimental: empty.clone(),
            extensions: empty,
        }
    }
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpIconDto {
    /// Verified icon source URI, retained as exact source text.
    pub src: json_projection::Utf16JsonProjection,
    /// Optional MIME type for the icon.
    pub mime_type: Option<String>,
    /// Declared size labels.
    pub sizes: Vec<String>,
    /// Optional theme discriminator.
    pub theme: Option<String>,
    /// Unrecognized extension members preserved as an exact source object
    /// across transport/cache hops.
    pub extra: json_projection::Utf16JsonProjection,
}

/// Optional MCP tool annotations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolAnnotationsDto {
    /// Optional user-facing title override, retained as exact source text.
    pub title: Option<json_projection::Utf16JsonProjection>,
    /// Whether the tool is host-proven read-only.
    pub read_only_hint: Option<bool>,
    /// Whether the tool may be destructive.
    pub destructive_hint: Option<bool>,
    /// Whether the host can prove the tool is idempotent.
    pub idempotent_hint: Option<bool>,
    /// Whether the tool touches the outside world.
    pub open_world_hint: Option<bool>,
    /// Unrecognized annotation hints preserved as an exact source object
    /// across transport/cache hops.
    pub extra: json_projection::Utf16JsonProjection,
}

impl Default for McpToolAnnotationsDto {
    fn default() -> Self {
        Self {
            title: None,
            read_only_hint: None,
            destructive_hint: None,
            idempotent_hint: None,
            open_world_hint: None,
            extra: empty_source_json_object(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpIconKnownDto {
    #[serde(deserialize_with = "deserialize_required_mcp_string")]
    src: json_projection::Utf16JsonProjection,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    sizes: Vec<String>,
    #[serde(default)]
    theme: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct McpIconWireDto<'a> {
    #[serde(serialize_with = "serialize_required_mcp_string")]
    src: json_projection::Utf16JsonProjection,
    #[serde(skip_serializing_if = "Option::is_none")]
    mime_type: Option<&'a str>,
    #[serde(skip_serializing_if = "is_empty_slice")]
    sizes: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    theme: Option<&'a str>,
}

impl Serialize for McpIconDto {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        const KNOWN_FIELDS: &[&str] = &["src", "mimeType", "sizes", "theme"];
        let known_json = serde_json::to_string(&McpIconWireDto {
            src: self.src.clone(),
            mime_type: self.mime_type.as_deref(),
            sizes: &self.sizes,
            theme: self.theme.as_deref(),
        })
        .map_err(serde::ser::Error::custom)?;
        let known = json_projection::Utf16JsonProjection::parse(&known_json)
            .map_err(serde::ser::Error::custom)?;
        let source = merge_source_object_fields(&known, &self.extra, KNOWN_FIELDS)
            .map_err(serde::ser::Error::custom)?;
        json_projection::raw_json_serde::serialize(&source, serializer)
    }
}

impl<'de> Deserialize<'de> for McpIconDto {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        const KNOWN_FIELDS: &[&str] = &["src", "mimeType", "sizes", "theme"];
        let source = json_projection::raw_json_serde::deserialize(deserializer)?;
        let known_source = source
            .pick_object_fields(KNOWN_FIELDS)
            .map_err(serde::de::Error::custom)?;
        let known_json = known_source
            .to_json_string()
            .map_err(serde::de::Error::custom)?;
        let known: McpIconKnownDto =
            serde_json::from_str(&known_json).map_err(serde::de::Error::custom)?;
        let extra = source_object_without_fields(&source, KNOWN_FIELDS)
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            src: known.src,
            mime_type: known.mime_type,
            sizes: known.sizes,
            theme: known.theme,
            extra,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpToolAnnotationsKnownDto {
    #[serde(default, deserialize_with = "deserialize_optional_mcp_string")]
    title: Option<json_projection::Utf16JsonProjection>,
    #[serde(default)]
    read_only_hint: Option<bool>,
    #[serde(default)]
    destructive_hint: Option<bool>,
    #[serde(default)]
    idempotent_hint: Option<bool>,
    #[serde(default)]
    open_world_hint: Option<bool>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct McpToolAnnotationsWireDto {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_mcp_string"
    )]
    title: Option<json_projection::Utf16JsonProjection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    read_only_hint: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    destructive_hint: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    idempotent_hint: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    open_world_hint: Option<bool>,
}

impl Serialize for McpToolAnnotationsDto {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        const KNOWN_FIELDS: &[&str] = &[
            "title",
            "readOnlyHint",
            "destructiveHint",
            "idempotentHint",
            "openWorldHint",
        ];
        let known_json = serde_json::to_string(&McpToolAnnotationsWireDto {
            title: self.title.clone(),
            read_only_hint: self.read_only_hint,
            destructive_hint: self.destructive_hint,
            idempotent_hint: self.idempotent_hint,
            open_world_hint: self.open_world_hint,
        })
        .map_err(serde::ser::Error::custom)?;
        let known = json_projection::Utf16JsonProjection::parse(&known_json)
            .map_err(serde::ser::Error::custom)?;
        let source = merge_source_object_fields(&known, &self.extra, KNOWN_FIELDS)
            .map_err(serde::ser::Error::custom)?;
        json_projection::raw_json_serde::serialize(&source, serializer)
    }
}

impl<'de> Deserialize<'de> for McpToolAnnotationsDto {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        const KNOWN_FIELDS: &[&str] = &[
            "title",
            "readOnlyHint",
            "destructiveHint",
            "idempotentHint",
            "openWorldHint",
        ];
        let source = json_projection::raw_json_serde::deserialize(deserializer)?;
        let known_source = source
            .pick_object_fields(KNOWN_FIELDS)
            .map_err(serde::de::Error::custom)?;
        let known_json = known_source
            .to_json_string()
            .map_err(serde::de::Error::custom)?;
        let known: McpToolAnnotationsKnownDto =
            serde_json::from_str(&known_json).map_err(serde::de::Error::custom)?;
        let extra = source_object_without_fields(&source, KNOWN_FIELDS)
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            title: known.title,
            read_only_hint: known.read_only_hint,
            destructive_hint: known.destructive_hint,
            idempotent_hint: known.idempotent_hint,
            open_world_hint: known.open_world_hint,
            extra,
        })
    }
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

/// Standard MCP 2025-11-25 `Tool` wire definition with opaque metadata retained.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolDefinitionDto {
    /// Raw tool name exposed by the server.
    pub name: String,
    /// Optional user-facing title, retained as exact source text.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_mcp_string",
        deserialize_with = "deserialize_optional_mcp_string"
    )]
    pub title: Option<json_projection::Utf16JsonProjection>,
    /// Optional user-facing description retained as its exact JSON source.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "json_projection::raw_json_serde::serialize_option",
        deserialize_with = "json_projection::raw_json_serde::deserialize_option"
    )]
    pub description: Option<json_projection::Utf16JsonProjection>,
    /// Input JSON Schema preserved directly from the server's raw JSON.
    #[serde(with = "json_projection::raw_json_serde")]
    pub input_schema: json_projection::Utf16JsonProjection,
    /// Optional structured output JSON Schema.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "json_projection::raw_json_serde::serialize_option",
        deserialize_with = "json_projection::raw_json_serde::deserialize_option"
    )]
    pub output_schema: Option<json_projection::Utf16JsonProjection>,
    /// Optional derived annotations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<McpToolAnnotationsDto>,
    /// Optional execution metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<McpToolExecutionDto>,
    /// Optional icon list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub icons: Vec<McpIconDto>,
    /// Opaque vendor metadata preserved directly from the server's raw JSON.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "json_projection::raw_json_serde::serialize_option",
        deserialize_with = "json_projection::raw_json_serde::deserialize_option"
    )]
    pub meta: Option<json_projection::Utf16JsonProjection>,
}

impl McpToolDefinitionDto {
    /// Construct a minimal wire `Tool` definition.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        input_schema: json_projection::Utf16JsonProjection,
    ) -> Self {
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

    /// Exact source schema from the MCP definition.
    #[must_use]
    pub const fn input_schema(&self) -> &json_projection::Utf16JsonProjection {
        &self.input_schema
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
    /// Root JSON string description shown to the model, retained as source.
    #[serde(
        serialize_with = "json_projection::raw_json_serde::serialize",
        deserialize_with = "deserialize_mcp_tool_description"
    )]
    pub description: json_projection::Utf16JsonProjection,
    /// JSON-Schema for the tool's input, retained as raw source JSON.
    #[serde(with = "json_projection::raw_json_serde")]
    pub input_schema: json_projection::Utf16JsonProjection,
    /// Optional JSON-Schema for the tool's structured output.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "json_projection::raw_json_serde::serialize_option",
        deserialize_with = "json_projection::raw_json_serde::deserialize_option"
    )]
    pub output_schema: Option<json_projection::Utf16JsonProjection>,
    /// Optional standardized tool annotations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<McpToolAnnotationsDto>,
    /// Optional icon list preserved from the upstream definition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub icons: Vec<McpIconDto>,
    /// Opaque vendor metadata preserved as raw source JSON.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "json_projection::raw_json_serde::serialize_option",
        deserialize_with = "json_projection::raw_json_serde::deserialize_option"
    )]
    pub meta: Option<json_projection::Utf16JsonProjection>,
    /// Engine-side fully-qualified identifier (`mcp__<server>__<tool>`).
    pub full_name: String,
    /// Retrieval prefilter hint from `tool._meta['anthropic/searchHint']`
    /// (e.g. `"shell"`, `"editor"`). `None` when absent or when the server
    /// did not set `_meta`. Forwarded from [`mcp::client::ToolMeta`] per
    /// `client.ts:1777-1778`. Used by tool-search ranking to prefer tools
    /// whose `searchHint` matches the current task description.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "json_projection::raw_json_serde::serialize_option",
        deserialize_with = "json_projection::raw_json_serde::deserialize_option"
    )]
    pub search_hint: Option<json_projection::Utf16JsonProjection>,
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

fn deserialize_mcp_tool_description<'de, D>(
    deserializer: D,
) -> Result<json_projection::Utf16JsonProjection, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let description = json_projection::raw_json_serde::deserialize(deserializer)?;
    if description.string_units("").is_some() {
        Ok(description)
    } else {
        Err(serde::de::Error::custom(
            "MCP tool description must be a JSON string",
        ))
    }
}

impl McpToolDto {
    /// Raw upstream tool name.
    #[must_use]
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    /// Model-facing tool description.
    #[must_use]
    pub fn description(&self) -> &json_projection::Utf16JsonProjection {
        &self.description
    }

    /// Input JSON Schema advertised for the tool.
    #[must_use]
    pub const fn input_schema(&self) -> &json_projection::Utf16JsonProjection {
        &self.input_schema
    }

    /// Exact source output schema from the MCP definition, when supplied.
    #[must_use]
    pub const fn output_schema(&self) -> Option<&json_projection::Utf16JsonProjection> {
        self.output_schema.as_ref()
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
    #[serde(
        serialize_with = "serialize_required_mcp_string",
        deserialize_with = "deserialize_required_mcp_string"
    )]
    pub uri: json_projection::Utf16JsonProjection,
    /// Human-readable name.
    #[serde(
        serialize_with = "serialize_required_mcp_string",
        deserialize_with = "deserialize_required_mcp_string"
    )]
    pub name: json_projection::Utf16JsonProjection,
    /// Optional human-readable description.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_mcp_string",
        deserialize_with = "deserialize_optional_mcp_string"
    )]
    pub description: Option<json_projection::Utf16JsonProjection>,
    /// Optional content type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Opaque vendor metadata retained as raw source JSON.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "json_projection::raw_json_serde::serialize_option",
        deserialize_with = "json_projection::raw_json_serde::deserialize_option"
    )]
    pub meta: Option<json_projection::Utf16JsonProjection>,
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
    #[serde(
        rename = "uriTemplate",
        serialize_with = "serialize_required_mcp_string",
        deserialize_with = "deserialize_required_mcp_string"
    )]
    pub uri_template: json_projection::Utf16JsonProjection,
    /// Human-readable name.
    #[serde(
        serialize_with = "serialize_required_mcp_string",
        deserialize_with = "deserialize_required_mcp_string"
    )]
    pub name: json_projection::Utf16JsonProjection,
    /// Optional human-readable description.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_mcp_string",
        deserialize_with = "deserialize_optional_mcp_string"
    )]
    pub description: Option<json_projection::Utf16JsonProjection>,
    /// Optional content type.
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Optional resource annotations preserved as raw source JSON.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "json_projection::raw_json_serde::serialize_option",
        deserialize_with = "json_projection::raw_json_serde::deserialize_option"
    )]
    pub annotations: Option<json_projection::Utf16JsonProjection>,
    /// Opaque vendor metadata retained from the raw source.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "json_projection::raw_json_serde::serialize_option",
        deserialize_with = "json_projection::raw_json_serde::deserialize_option"
    )]
    pub meta: Option<json_projection::Utf16JsonProjection>,
}

/// One prompt advertised by an MCP server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpPromptDto {
    /// Prompt name as exposed by the server.
    #[serde(
        serialize_with = "serialize_required_mcp_string",
        deserialize_with = "deserialize_required_mcp_string"
    )]
    pub name: json_projection::Utf16JsonProjection,
    /// Optional human-readable description.
    #[serde(
        default,
        serialize_with = "serialize_optional_mcp_string",
        deserialize_with = "deserialize_optional_mcp_string"
    )]
    pub description: Option<json_projection::Utf16JsonProjection>,
    /// Named arguments accepted by `prompts/get`, in server-declared order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<McpPromptArgumentDto>,
}

/// One named argument declared by an MCP prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpPromptArgumentDto {
    /// Wire argument name sent to `prompts/get`.
    #[serde(
        serialize_with = "serialize_required_mcp_string",
        deserialize_with = "deserialize_required_mcp_string"
    )]
    pub name: json_projection::Utf16JsonProjection,
    /// Optional human-readable description.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_optional_mcp_string",
        deserialize_with = "deserialize_optional_mcp_string"
    )]
    pub description: Option<json_projection::Utf16JsonProjection>,
    /// Whether the server requires this argument.
    #[serde(default)]
    pub required: bool,
}

/// Result of a tool invocation.
///
/// The MCP `CallToolResult` wire shape can carry optional arbitrary JSON
/// members alongside `content`/`isError`. Native reads `error` when an error
/// result has no usable content, and forwards `_meta` / `structuredContent`.
/// These members stay as source projections so explicit null and exact UTF-16
/// string units survive decoding.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolResultDto {
    /// JSON content returned by the tool, kept as raw source JSON.
    #[serde(with = "json_projection::raw_json_serde")]
    pub content: json_projection::Utf16JsonProjection,
    /// True when the server flagged the result as an error.
    pub is_error: bool,
    /// Server-supplied `error` value used by Native's error-detail fallback.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "json_projection::raw_json_serde::serialize_option",
        deserialize_with = "json_projection::raw_json_serde::deserialize_option"
    )]
    pub error: Option<json_projection::Utf16JsonProjection>,
    /// Server-supplied `_meta` block retained as raw source JSON.
    #[serde(
        rename = "_meta",
        skip_serializing_if = "Option::is_none",
        default,
        serialize_with = "json_projection::raw_json_serde::serialize_option",
        deserialize_with = "json_projection::raw_json_serde::deserialize_option"
    )]
    pub meta: Option<json_projection::Utf16JsonProjection>,
    /// Server-supplied `structuredContent` retained as raw source JSON.
    #[serde(
        rename = "structuredContent",
        skip_serializing_if = "Option::is_none",
        default,
        serialize_with = "json_projection::raw_json_serde::serialize_option",
        deserialize_with = "json_projection::raw_json_serde::deserialize_option"
    )]
    pub structured_content: Option<json_projection::Utf16JsonProjection>,
}

impl Default for McpToolResultDto {
    fn default() -> Self {
        Self {
            content: json_projection::Utf16JsonProjection::parse("null")
                .expect("null is valid JSON"),
            is_error: false,
            error: None,
            meta: None,
            structured_content: None,
        }
    }
}

/// Arbitrary notification pushed by a server (logging, progress, etc).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpNotificationDto {
    /// JSON-RPC method.
    pub method: String,
    /// Method-specific parameters.
    #[serde(with = "json_projection::raw_json_serde")]
    pub params: json_projection::Utf16JsonProjection,
}

/// Server-initiated elicitation request (asks the host to gather user input).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitRequestDto {
    /// Server-supplied request parameters.
    #[serde(with = "json_projection::raw_json_serde")]
    pub params: json_projection::Utf16JsonProjection,
}

/// Reply to an [`ElicitRequestDto`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElicitResultDto {
    /// User-supplied data being returned to the server.
    #[serde(with = "json_projection::raw_json_serde")]
    pub data: json_projection::Utf16JsonProjection,
}

/// Contents read from an MCP resource.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResourceContentDto {
    /// Echoed URI.
    #[serde(
        serialize_with = "serialize_required_mcp_string",
        deserialize_with = "deserialize_required_mcp_string"
    )]
    pub uri: json_projection::Utf16JsonProjection,
    /// Resource body (text-encoded; binaries are base64).
    #[serde(
        serialize_with = "serialize_required_mcp_string",
        deserialize_with = "deserialize_required_mcp_string"
    )]
    pub content: json_projection::Utf16JsonProjection,
    /// Optional content type for the returned body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Opaque vendor metadata preserved byte-for-byte.
    #[serde(
        rename = "_meta",
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "json_projection::raw_json_serde::serialize_option",
        deserialize_with = "json_projection::raw_json_serde::deserialize_option"
    )]
    pub meta: Option<json_projection::Utf16JsonProjection>,
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
    #[serde(
        serialize_with = "serialize_required_mcp_string",
        deserialize_with = "deserialize_required_mcp_string"
    )]
    pub uri: json_projection::Utf16JsonProjection,
    /// MIME type as advertised by the server (absent on the wire → `None`).
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none", default)]
    pub mime_type: Option<String>,
    /// Opaque vendor metadata retained as raw source JSON.
    #[serde(
        rename = "_meta",
        skip_serializing_if = "Option::is_none",
        default,
        serialize_with = "json_projection::raw_json_serde::serialize_option",
        deserialize_with = "json_projection::raw_json_serde::deserialize_option"
    )]
    pub meta: Option<json_projection::Utf16JsonProjection>,
    /// Text content of the block (text blocks; also the human-readable
    /// "saved to disk" message for persisted blobs).
    #[serde(
        skip_serializing_if = "Option::is_none",
        default,
        serialize_with = "serialize_optional_mcp_string",
        deserialize_with = "deserialize_optional_mcp_string"
    )]
    pub text: Option<json_projection::Utf16JsonProjection>,
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

    /// Metadata for this live connection, captured by initialize or discovery.
    fn server_metadata(&self, _id: McpConnectionId) -> Option<McpServerMetadataDto> {
        None
    }

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
        input: json_projection::Utf16JsonProjection,
    ) -> Result<McpToolResultDto, McpError>;

    /// Read the contents of a resource by its exact source URI.
    async fn read_resource(
        &self,
        conn: &McpRawConnection,
        uri: &json_projection::Utf16JsonProjection,
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
        uri: &json_projection::Utf16JsonProjection,
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
        McpIconDto, McpPromptDto, McpResourceContentDto, McpResourceContentsRich, McpResourceDto,
        McpResourceTemplateDto, McpServerMetadataDto, McpToolAnnotationsDto, McpToolDefinitionDto,
        McpToolDto, McpToolResultDto, McpTransportSpec,
    };
    use json_projection::Utf16JsonProjection;
    use serde_json::json;

    fn project(input: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(input).expect("valid source JSON fixture")
    }

    fn project_utf16(input: &[u16]) -> Utf16JsonProjection {
        Utf16JsonProjection::parse_utf16_json(input).expect("valid UTF-16 source JSON fixture")
    }

    #[test]
    fn mcp_tool_dto_preserves_optional_metadata_fields() {
        let value = serde_json::to_value(McpToolDto {
            server_name: "fixture".to_string(),
            tool_name: "save_item".to_string(),
            description: project(r#""Save one fixture entry.""#),
            input_schema: project(r#"{"type":"object"}"#),
            output_schema: Some(project(
                r#"{"type":"object","properties":{"ok":{"type":"boolean"}}}"#,
            )),
            annotations: Some(McpToolAnnotationsDto {
                title: Some("Save habit".into()),
                read_only_hint: Some(false),
                destructive_hint: Some(false),
                idempotent_hint: Some(true),
                open_world_hint: Some(false),
                extra: project(r#"{"vendor/riskTier":"reviewed"}"#),
            }),
            icons: vec![McpIconDto {
                src: "https://example.invalid/icon.svg".into(),
                mime_type: Some("image/svg+xml".to_string()),
                sizes: vec!["64x64".to_string()],
                theme: None,
                extra: project(r#"{"vendor/accent":"blue"}"#),
            }],
            meta: Some(project(
                r#"{"openai/outputTemplate":"ui://fixture/widget.html"}"#,
            )),
            full_name: "mcp__fixture__save_item".to_string(),
            search_hint: Some(project(r#""fixture items""#)),
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
            "ui://fixture/widget.html"
        );
    }

    #[test]
    fn mcp_tool_description_and_search_hint_keep_exact_source_strings() {
        let tool: McpToolDto = serde_json::from_str(
            r#"{"server_name":"s","tool_name":"t","description":"\ud800","input_schema":{},"full_name":"mcp__s__t","search_hint":"\udfff"}"#,
        )
        .expect("source tool description and search hint");
        assert_eq!(tool.description().string_units(""), Some(vec![0xd800]));
        assert_eq!(
            tool.search_hint
                .as_ref()
                .and_then(|hint| hint.string_units("")),
            Some(vec![0xdfff])
        );
        let encoded = serde_json::to_string(&tool).expect("serialize source tool");
        assert!(encoded.contains("\"description\":\"\\ud800\""));
        assert!(encoded.contains("\"search_hint\":\"\\udfff\""));

        let absent: McpToolDefinitionDto =
            serde_json::from_str(r#"{"name":"no-description","inputSchema":{}}"#).unwrap();
        let explicit_null: McpToolDefinitionDto = serde_json::from_str(
            r#"{"name":"null-description","description":null,"inputSchema":{}}"#,
        )
        .unwrap();
        assert!(absent.description.is_none());
        assert!(explicit_null
            .description
            .as_ref()
            .and_then(|value| value.at_pointer(""))
            .is_some_and(|value| value.is_null()));
    }

    #[test]
    fn mcp_source_tool_and_result_dtos_preserve_lone_surrogates_and_nulls() {
        let definition: McpToolDefinitionDto = serde_json::from_str(
            r#"{"name":"source","title":"\udfff","description":"\ud800","inputSchema":{"description":"\ud800"},"outputSchema":null,"_meta":{"opaque":"\udfff"}}"#,
        )
        .expect("raw source tool definition");
        assert_eq!(
            definition
                .title
                .as_ref()
                .and_then(|title| title.string_units("")),
            Some(vec![0xdfff])
        );
        assert_eq!(
            definition
                .description
                .as_ref()
                .and_then(|description| description.string_units("")),
            Some(vec![0xd800])
        );
        assert_eq!(
            definition.input_schema.string_units("/description"),
            Some(vec![0xd800])
        );
        assert_eq!(
            definition
                .output_schema
                .as_ref()
                .and_then(|schema| schema.at_pointer(""))
                .map(|value| value.is_null()),
            Some(true)
        );
        assert_eq!(
            definition
                .meta
                .as_ref()
                .and_then(|meta| meta.string_units("/opaque")),
            Some(vec![0xdfff])
        );
        let definition_json =
            serde_json::to_string(&definition).expect("serialize source definition");
        assert!(definition_json.contains(r#""title":"\udfff""#));
        let definition_round_trip: McpToolDefinitionDto =
            serde_json::from_str(&definition_json).expect("round-trip source definition");
        assert_eq!(definition_round_trip.title, definition.title);

        let result: McpToolResultDto = serde_json::from_str(
            r#"{"content":{"text":"\ud800"},"is_error":true,"error":"\udfff","_meta":null,"structuredContent":{"value":"\udfff"}}"#,
        )
        .expect("raw source tool result");
        assert_eq!(result.content.string_units("/text"), Some(vec![0xd800]));
        assert_eq!(
            result
                .error
                .as_ref()
                .and_then(|error| error.string_units("")),
            Some(vec![0xdfff])
        );
        assert_eq!(
            result
                .meta
                .as_ref()
                .and_then(|meta| meta.at_pointer(""))
                .map(|value| value.is_null()),
            Some(true)
        );
        assert_eq!(
            result
                .structured_content
                .as_ref()
                .and_then(|content| content.string_units("/value")),
            Some(vec![0xdfff])
        );

        let serialized = serde_json::to_string(&result).expect("serialize source result");
        let round_trip: McpToolResultDto =
            serde_json::from_str(&serialized).expect("round-trip raw source result");
        assert_eq!(round_trip.content.string_units("/text"), Some(vec![0xd800]));
        assert_eq!(
            round_trip
                .error
                .as_ref()
                .and_then(|error| error.string_units("")),
            Some(vec![0xdfff])
        );
        assert_eq!(
            round_trip
                .structured_content
                .as_ref()
                .and_then(|content| content.string_units("/value")),
            Some(vec![0xdfff])
        );
    }

    #[test]
    fn mcp_tool_result_error_preserves_absent_vs_explicit_null() {
        let absent: McpToolResultDto =
            serde_json::from_str(r#"{"content":[],"is_error":true}"#).expect("absent error field");
        assert!(absent.error.is_none());
        assert!(!serde_json::to_string(&absent)
            .expect("serialize absent error")
            .contains("\"error\""));

        let explicit_null: McpToolResultDto =
            serde_json::from_str(r#"{"content":[],"is_error":true,"error":null}"#)
                .expect("explicit null error field");
        assert_eq!(
            explicit_null
                .error
                .as_ref()
                .and_then(|error| error.at_pointer(""))
                .map(|value| value.is_null()),
            Some(true)
        );
        assert!(serde_json::to_string(&explicit_null)
            .expect("serialize explicit null error")
            .contains(r#""error":null"#));
    }

    #[test]
    fn mcp_capabilities_and_notifications_preserve_exact_source_json() {
        let mut extension_units: Vec<u16> = "{\"".encode_utf16().collect();
        extension_units.push(0xd800);
        extension_units.extend("\":{\"opaque\":\"".encode_utf16());
        extension_units.push(0xdfff);
        extension_units.extend("\"}}".encode_utf16());
        let mut capabilities = super::ServerCapabilitiesDto::default();
        capabilities.extensions = project_utf16(&extension_units);

        let encoded = serde_json::to_string(&capabilities).expect("serialize capabilities");
        let decoded: super::ServerCapabilitiesDto =
            serde_json::from_str(&encoded).expect("deserialize capabilities");
        let entry = decoded
            .extensions
            .object_entries()
            .expect("extension source object")
            .into_iter()
            .next()
            .expect("extension entry");
        assert_eq!(entry.code_units, &[0xd800]);
        assert_eq!(
            decoded
                .extensions
                .subprojection_property_units(&[0xd800])
                .unwrap()
                .string_units("/opaque"),
            Some(vec![0xdfff])
        );

        let mut params_units: Vec<u16> = "{\"x\":\"".encode_utf16().collect();
        params_units.push(0xd800);
        params_units.extend("\"}".encode_utf16());
        let notification = super::McpNotificationDto {
            method: "notifications/message".into(),
            params: project_utf16(&params_units),
        };
        let encoded = serde_json::to_string(&notification).expect("serialize notification");
        let decoded: super::McpNotificationDto =
            serde_json::from_str(&encoded).expect("deserialize notification");
        assert_eq!(decoded.params.string_units("/x"), Some(vec![0xd800]));
    }

    #[test]
    fn mcp_instructions_descriptions_and_resource_text_preserve_exact_units() {
        let metadata: McpServerMetadataDto = serde_json::from_str(r#"{"instructions":"\ud800"}"#)
            .expect("server instructions with lone surrogate");
        assert_eq!(
            metadata
                .instructions
                .as_ref()
                .and_then(|text| text.string_units("")),
            Some(vec![0xd800])
        );
        let metadata_json = serde_json::to_string(&metadata).expect("serialize metadata");
        assert!(metadata_json.contains(r#""instructions":"\ud800""#));

        let resource: McpResourceDto = serde_json::from_str(
            r#"{"uri":"resource://exact","name":"resource","description":"left\ud800right"}"#,
        )
        .expect("resource description with lone surrogate");
        assert_eq!(
            resource
                .description
                .as_ref()
                .and_then(|text| text.string_units("")),
            Some(vec![
                b'l' as u16,
                b'e' as u16,
                b'f' as u16,
                b't' as u16,
                0xd800,
                b'r' as u16,
                b'i' as u16,
                b'g' as u16,
                b'h' as u16,
                b't' as u16,
            ])
        );
        let resource_json = serde_json::to_string(&resource).expect("serialize resource");
        let resource_round_trip: McpResourceDto =
            serde_json::from_str(&resource_json).expect("round-trip resource");
        assert_eq!(resource_round_trip.description, resource.description);

        let template: McpResourceTemplateDto = serde_json::from_str(
            r#"{"uriTemplate":"file:///{path}","name":"file","description":"\ud800"}"#,
        )
        .expect("resource-template description with lone surrogate");
        assert_eq!(
            template
                .description
                .as_ref()
                .and_then(|text| text.string_units("")),
            Some(vec![0xd800])
        );

        let prompt: McpPromptDto = serde_json::from_str(
            r#"{"name":"prompt","description":"\udfff","arguments":[{"name":"arg","description":"\ud800","required":true}]}"#,
        )
        .expect("prompt and argument descriptions with lone surrogates");
        assert_eq!(
            prompt.name.string_units(""),
            Some("prompt".encode_utf16().collect())
        );
        assert_eq!(
            prompt
                .description
                .as_ref()
                .and_then(|text| text.string_units("")),
            Some(vec![0xdfff])
        );
        assert_eq!(
            prompt.arguments[0]
                .description
                .as_ref()
                .and_then(|text| text.string_units("")),
            Some(vec![0xd800])
        );
        assert_eq!(
            prompt.arguments[0].name.string_units(""),
            Some("arg".encode_utf16().collect())
        );

        let content: McpResourceContentDto =
            serde_json::from_str(r#"{"uri":"resource://exact","content":"\ud800"}"#)
                .expect("resource text content with lone surrogate");
        assert_eq!(content.content.string_units(""), Some(vec![0xd800]));
        let content_json = serde_json::to_string(&content).expect("serialize resource content");
        assert!(content_json.contains(r#""content":"\ud800""#));
        let content_round_trip: McpResourceContentDto =
            serde_json::from_str(&content_json).expect("round-trip resource content");
        assert_eq!(content_round_trip.content, content.content);
        assert!(serde_json::from_str::<McpResourceContentDto>(
            r#"{"uri":"resource://exact","content":17}"#
        )
        .is_err());
        let rich: McpResourceContentsRich =
            serde_json::from_str(r#"{"uri":"resource://exact","text":"\udfff"}"#)
                .expect("rich resource text with lone surrogate");
        assert_eq!(
            rich.text.as_ref().and_then(|text| text.string_units("")),
            Some(vec![0xdfff])
        );

        let null_text: McpResourceDto = serde_json::from_str(
            r#"{"uri":"resource://exact","name":"resource","description":null}"#,
        )
        .expect("null description remains absent");
        assert!(null_text.description.is_none());
        assert!(serde_json::from_str::<McpResourceDto>(
            r#"{"uri":"resource://exact","name":"resource","description":17}"#
        )
        .is_err());
    }

    #[test]
    fn mcp_icon_and_annotation_extras_keep_exact_keys_and_nested_values() {
        let annotations: McpToolAnnotationsDto = serde_json::from_str(
            r#"{"title":"\ud800","readOnlyHint":true,"vendor-\ud800":{"nested":"\udfff"},"__proto__":{"safe":true}}"#,
        )
        .expect("annotations with source extras");
        assert_eq!(
            annotations
                .title
                .as_ref()
                .and_then(|title| title.string_units("")),
            Some(vec![0xd800])
        );
        let annotation_key = "vendor-".encode_utf16().chain([0xd800]).collect::<Vec<_>>();
        assert_eq!(
            annotations
                .extra
                .subprojection_property_units(&annotation_key)
                .expect("exact annotation key")
                .string_units("/nested"),
            Some(vec![0xdfff])
        );
        let encoded = serde_json::to_string(&annotations).expect("serialize annotations");
        assert!(!encoded.contains("\"nodes\""));
        assert!(encoded.contains(r#""title":"\ud800""#));
        assert!(encoded.contains(r#""readOnlyHint":true"#));
        assert!(encoded.contains(r#""vendor-\ud800":{"nested":"\udfff"}"#));
        assert!(encoded.contains(r#""__proto__":{"safe":true}"#));
        let round_trip: McpToolAnnotationsDto =
            serde_json::from_str(&encoded).expect("round-trip annotation extras");
        assert_eq!(round_trip.extra, annotations.extra);

        let icon: McpIconDto = serde_json::from_str(
            r#"{"src":"https://example.invalid/icon-\ud800.svg","vendor-\udfff":{"text":"\ud800"}}"#,
        )
        .expect("icon with source extra");
        assert_eq!(
            icon.src.string_units(""),
            Some(
                "https://example.invalid/icon-"
                    .encode_utf16()
                    .chain([0xd800])
                    .chain(".svg".encode_utf16())
                    .collect()
            )
        );
        let icon_key = "vendor-".encode_utf16().chain([0xdfff]).collect::<Vec<_>>();
        assert_eq!(
            icon.extra
                .subprojection_property_units(&icon_key)
                .expect("exact icon key")
                .string_units("/text"),
            Some(vec![0xd800])
        );
        let encoded = serde_json::to_string(&icon).expect("serialize icon");
        assert!(!encoded.contains("\"nodes\""));
        assert!(encoded.contains(r#""src":"https://example.invalid/icon-\ud800.svg""#));
        assert!(encoded.contains(r#""vendor-\udfff":{"text":"\ud800"}"#));
        let round_trip: McpIconDto =
            serde_json::from_str(&encoded).expect("round-trip icon extras");
        assert_eq!(round_trip.extra, icon.extra);
    }

    #[test]
    fn mcp_resource_and_prompt_routing_text_preserve_exact_units() {
        let resource: McpResourceDto = serde_json::from_str(
            r#"{"uri":"resource://x/\ud800","name":"name\udfff","description":"desc"}"#,
        )
        .expect("resource identifiers with lone surrogates");
        assert_eq!(
            resource.uri.string_units(""),
            Some("resource://x/".encode_utf16().chain([0xd800]).collect())
        );
        assert_eq!(
            resource.name.string_units(""),
            Some("name".encode_utf16().chain([0xdfff]).collect())
        );
        let resource_json = serde_json::to_string(&resource).expect("serialize source resource");
        assert!(resource_json.contains(r#""uri":"resource://x/\ud800""#));
        assert!(resource_json.contains(r#""name":"name\udfff""#));
        let resource_round_trip: McpResourceDto =
            serde_json::from_str(&resource_json).expect("round-trip resource identity");
        assert_eq!(resource_round_trip.uri, resource.uri);
        assert_eq!(resource_round_trip.name, resource.name);

        let template: McpResourceTemplateDto =
            serde_json::from_str(r#"{"uriTemplate":"file:///\ud800/{x}","name":"template\udfff"}"#)
                .expect("resource template identifiers with lone surrogates");
        assert_eq!(
            template.uri_template.string_units(""),
            Some(
                "file:///"
                    .encode_utf16()
                    .chain([0xd800])
                    .chain("/{x}".encode_utf16())
                    .collect()
            )
        );
        assert_eq!(
            template.name.string_units(""),
            Some("template".encode_utf16().chain([0xdfff]).collect())
        );

        let prompt: McpPromptDto = serde_json::from_str(
            r#"{"name":"prompt\ud800","arguments":[{"name":"arg\udfff","required":true}]}"#,
        )
        .expect("prompt routing names with lone surrogates");
        assert_eq!(
            prompt.name.string_units(""),
            Some("prompt".encode_utf16().chain([0xd800]).collect())
        );
        assert_eq!(
            prompt.arguments[0].name.string_units(""),
            Some("arg".encode_utf16().chain([0xdfff]).collect())
        );
        let prompt_json = serde_json::to_string(&prompt).expect("serialize source prompt");
        assert!(prompt_json.contains(r#""name":"prompt\ud800""#));
        assert!(prompt_json.contains(r#""name":"arg\udfff""#));
        let prompt_round_trip: McpPromptDto =
            serde_json::from_str(&prompt_json).expect("round-trip prompt names");
        assert_eq!(prompt_round_trip.name, prompt.name);
        assert_eq!(
            prompt_round_trip.arguments[0].name,
            prompt.arguments[0].name
        );

        let content: McpResourceContentDto =
            serde_json::from_str(r#"{"uri":"resource://read/\ud800","content":"body\udfff"}"#)
                .expect("read URI with lone surrogate");
        assert_eq!(
            content.uri.string_units(""),
            Some("resource://read/".encode_utf16().chain([0xd800]).collect())
        );
        let rich: McpResourceContentsRich =
            serde_json::from_str(r#"{"uri":"resource://rich/\udfff","text":"body"}"#)
                .expect("rich read URI with lone surrogate");
        assert_eq!(
            rich.uri.string_units(""),
            Some("resource://rich/".encode_utf16().chain([0xdfff]).collect())
        );
    }

    #[test]
    fn mcp_resource_dtos_preserve_description_mime_and_meta() {
        let resource = serde_json::to_value(McpResourceDto {
            uri: "ui://fixture/widget.html".into(),
            name: "Fixture widget".into(),
            description: Some("Interactive MCP App widget".into()),
            mime_type: Some("text/html;profile=mcp-app".to_string()),
            meta: Some(project(r#"{"openai/widgetPrefersBorder":true}"#)),
        })
        .expect("serialize resource dto");
        assert_eq!(resource["description"], "Interactive MCP App widget");
        assert_eq!(resource["mime_type"], "text/html;profile=mcp-app");
        assert_eq!(resource["_meta"]["openai/widgetPrefersBorder"], true);

        let template = serde_json::to_value(McpResourceTemplateDto {
            uri_template: "file:///{path}".into(),
            name: "Workspace file".into(),
            description: Some("One workspace file".into()),
            mime_type: Some("text/plain".to_string()),
            annotations: Some(project(r#"{"audience":["assistant"],"vendor/rank":7}"#)),
            meta: Some(project(r#"{"vendor/template":"opaque"}"#)),
        })
        .expect("serialize resource template dto");
        assert_eq!(template["annotations"]["vendor/rank"], 7);
        assert_eq!(template["_meta"]["vendor/template"], "opaque");

        let content = serde_json::to_value(McpResourceContentDto {
            uri: "ui://fixture/widget.html".into(),
            content: "<!doctype html>".into(),
            mime_type: Some("text/html;profile=mcp-app".to_string()),
            meta: Some(project(r#"{"openai/widgetDescription":"Fixture widget"}"#)),
        })
        .expect("serialize resource content dto");
        assert_eq!(content["mime_type"], "text/html;profile=mcp-app");
        assert_eq!(
            content["_meta"]["openai/widgetDescription"],
            "Fixture widget"
        );

        let rich = serde_json::to_value(McpResourceContentsRich {
            uri: "ui://fixture/widget.html".into(),
            mime_type: Some("text/html;profile=mcp-app".to_string()),
            meta: Some(project(r#"{"openai/widgetCSP":{"connect_domains":[]}}"#)),
            text: Some("<!doctype html>".into()),
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
    #[error("connection failed: {}", .0.string_at("").unwrap_or_default())]
    Connection(Utf16JsonProjection),
    /// MCP `initialize` handshake failed or returned an invalid response.
    #[error("handshake failed: {}", .0.string_at("").unwrap_or_default())]
    Handshake(Utf16JsonProjection),
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
    #[error("oauth flow failed: {}", .0.string_at("").unwrap_or_default())]
    OAuth(Utf16JsonProjection),
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
    /// Native modern result-driver failure, with its SDK code and data.
    #[error(transparent)]
    Result(McpSdkError),
    /// Catch-all for unexpected failures.
    #[error("internal error: {}", .0.string_at("").unwrap_or_default())]
    Internal(Utf16JsonProjection),
}

impl McpError {
    /// Return source text carried by free-form transport failures without
    /// lowering its UTF-16 units through the display formatter.
    #[must_use]
    pub fn source_diagnostic(&self) -> Option<Utf16JsonProjection> {
        match self {
            Self::Connection(message)
            | Self::Handshake(message)
            | Self::OAuth(message)
            | Self::Internal(message) => Some(message.clone()),
            _ => None,
        }
    }
}

/// An SDK result error, retaining its source-native structured data.
#[derive(Debug, Clone, PartialEq, Error)]
#[error("{message}")]
pub struct McpSdkError {
    /// Stable SDK error discriminator.
    pub code: &'static str,
    /// Human-readable diagnostic.
    pub message: String,
    /// Optional structured data retaining exact source units.
    pub data: Option<Utf16JsonProjection>,
}

impl McpSdkError {
    /// Construct an error without projecting its data through display JSON.
    pub fn new(
        code: &'static str,
        message: impl Into<String>,
        data: Option<Utf16JsonProjection>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            data,
        }
    }
}
