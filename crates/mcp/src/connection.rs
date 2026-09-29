//! Per-connection configuration, scope, and state machine.
//!
//! One MCP server is modelled as a config plus a state. The registry
//! (`registry.rs`) drives transitions between the variants below using
//! the platform-supplied [`lingxi_core::host::McpTransport`].

use lingxi_core::host::{
    McpConfiguredToolPolicyDto, McpPermissionCeiling, McpPromptDto, McpResourceDto,
    McpResourceTemplateDto, McpToolDto, McpTransportSpec, ServerCapabilitiesDto,
};
use lingxi_core::types::McpConnectionId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::SystemTime;

/// Stable source identity and cache provenance attached to a configured MCP
/// server.  This is deliberately MCP-local metadata: it never consults or
/// persists an LLM provider credential, profile, or account identifier.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerMetadata {
    /// Original transport label when the enum projection would lose it (for
    /// example `claudeai-proxy` is carried as an HTTP transport today).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// Optional Claude-compatible routing role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<McpServerRole>,
    /// Stable source of an agent inline MCP record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_source: Option<McpAgentSource>,
    /// True only when the host explicitly supplied this MCP config through a
    /// command-line option.
    #[serde(default, skip_serializing_if = "is_false")]
    pub cli_owned: bool,
    /// True only for an explicitly injected MCP temporary credential.
    #[serde(default, skip_serializing_if = "is_false")]
    pub ambient_credential: bool,
    /// This entry read the user's ENVIRONMENT: it carried a `${VAR}` reference
    /// on a field that expands, before expansion ran.
    ///
    /// Only ever set for an organization-delivered entry, and only there
    /// because it changes that entry's trust. A managed document that cannot
    /// read the user's environment is fully trusted and needs no
    /// `allowedMcpServers` entry; one that CAN is still checked against the
    /// list. See `enterprise_policy::org_delivered_needs_no_allowlist_entry`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub expanded_from_env: bool,
}

/// MCP server routing role.  `Comms` affects coordinator worker tool routing;
/// it is not an LLM provider or account concept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpServerRole {
    /// Keep this server's tools in the coordinator and hide them from workers.
    #[serde(rename = "comms")]
    Comms,
}

/// Stable source values used by Claude-compatible agent MCP records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum McpAgentSource {
    #[serde(rename = "built-in")]
    BuiltIn,
    #[serde(rename = "plugin")]
    Plugin,
    #[serde(rename = "userSettings")]
    UserSettings,
    #[serde(rename = "projectSettings")]
    ProjectSettings,
    /// No loader produces a local-tier agent today; present so the mapping from
    /// `AgentSource` stays total, and spelled the way the reference's
    /// `SettingSource` union spells it.
    #[serde(rename = "localSettings")]
    LocalSettings,
    #[serde(rename = "policySettings")]
    PolicySettings,
    #[serde(rename = "flagSettings")]
    FlagSettings,
    #[serde(rename = "additionalDirectory")]
    AdditionalDirectory,
}

/// Static configuration for one MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Logical server name (used as a map key and in approval prompts).
    pub name: String,
    /// Transport configuration consumed by the platform.
    pub spec: McpTransportSpec,
    /// Origin of the config, used by the approval policy.
    pub scope: ConfigScope,
    /// When true the registry must not auto-connect at startup.
    #[allow(dead_code)] // honoured by the connect loop in Plan 13
    pub disabled: bool,
    /// Per-server `tools/call` timeout (ms): the config `timeout` field, with
    /// the sse/http `request_timeout_ms` alias folded in at parse time (RAn:
    /// `timeout ??= min(request_timeout_ms, 300_000)`). `None` = no per-server
    /// override → the shared BHs resolver ([`crate::client::mcp_tool_timeout_for`])
    /// falls back to the `MCP_TOOL_TIMEOUT` env var / 100_000_000 default. Kept
    /// OFF [`McpTransportSpec`] on purpose so it never perturbs the
    /// `getServerKey`/`oauth::server_key` config hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Per-server discovery-cache preference from the parsed
    /// `discoveryCache` config key. `None` means the key was absent or the
    /// transport does not declare it; `Some(false)` is an explicit opt-out
    /// that purges any existing cache family and suppresses future reads and
    /// writes; `Some(true)` is an explicit opt-in that remains otherwise
    /// subject to the normal feature/transport/headers-helper gates.
    ///
    /// Kept OFF [`McpTransportSpec`] on purpose so it never perturbs the
    /// `getServerKey`/`oauth::server_key` config hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_cache: Option<bool>,
    /// `alwaysLoad`: force every tool from this server into the prompt, never
    /// deferred behind tool search ("Equivalent to setting defer_loading:false
    /// on the API"). OR'd into each tool's `always_load` bit at list time.
    #[serde(default, skip_serializing_if = "is_false")]
    pub always_load: bool,
    /// Optional per-tool rule producer declarations from config/UI state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<McpConfiguredToolPolicyDto>,
    /// Optional tighten-only per-tool ceilings from config/UI state.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        rename = "toolPermissions"
    )]
    pub tool_permissions: BTreeMap<String, McpPermissionCeiling>,
    /// Config-level error that makes the server unconnectable (claude
    /// `configError`, reason `url_invalid`): set at parse time when a remote
    /// entry's `url` expanded to an empty string. The server is KEPT in the
    /// inventory, but the connect path short-circuits to a failure WITHOUT
    /// dialing.
    ///
    /// This is claude's `INVALID_CONFIG`, not `UNCONFIGURED`: `klr`
    /// (@231828222) tags the case `configErrorReason:"url_invalid"`, so `zar`
    /// (@231408681) is false on both disjuncts and `Nxe` (@232117552) falls
    /// through the unconfigured gate into the next one →
    /// `errorCode:"INVALID_CONFIG"`. `Qee` (@231862992) is therefore false and
    /// `yEp` renders `✘ Failed to connect` with this text as the issue —
    /// `- Not configured` is reserved for [`McpServerConfig::is_unconfigured`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_error: Option<String>,
    /// Optional metadata used for cache identity and coordinator routing.
    /// Defaults to an empty value so old serialized configurations retain
    /// their exact logical-cache behavior.
    #[serde(default, skip_serializing_if = "McpServerMetadata::is_empty")]
    pub metadata: McpServerMetadata,
}

impl McpServerMetadata {
    /// Whether this is the compatibility/default metadata value.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// claude's error text for a server with nothing to dial (`Nxe`'s
/// `t.configError ?? "No URL configured for this server"`).
pub const UNCONFIGURED_ERROR: &str = "No URL configured for this server";

/// claude's byte-exact `configError` text for a non-blank but syntactically
/// malformed `url` (`Ve`/`Ae` @182283xxx: `try{new URL(t.url)}catch{C="'url'
/// is not a valid URL. Update the server's config and reconnect."}`). Unlike
/// [`UNCONFIGURED_ERROR`] (a blank url) this fires on a url that IS present
/// but does not parse — e.g. a bare hostname with no scheme. See
/// [`McpServerConfig::connect_time_url_error`].
pub const INVALID_URL_ERROR: &str =
    "'url' is not a valid URL. Update the server's config and reconnect.";

impl McpServerConfig {
    /// claude `zar` (@231408681) — is this server *unconfigured* (nothing to
    /// dial) rather than *misconfigured*? True when no [`Self::config_error`]
    /// was recorded and the transport carries a blank `url`. `Nxe` then skips
    /// the connect with `errorCode:"UNCONFIGURED"`, the ONLY case `mcp list` /
    /// `mcp get` render as `- Not configured` (`Qee` → `yEp`).
    ///
    /// `zar`'s other disjunct (`configErrorReason === "url_empty"`) is produced
    /// only by the plugin-MCP normalizer (@231445287), a path this port does
    /// not model, so the reason discriminator is not carried on the config.
    #[must_use]
    pub fn is_unconfigured(&self) -> bool {
        if self.config_error.is_some() {
            return false;
        }
        // `"url" in e` — the transports whose config object declares a `url`.
        let url = match &self.spec {
            McpTransportSpec::Sse { url, .. }
            | McpTransportSpec::Http { url, .. }
            | McpTransportSpec::WebSocket { url, .. }
            | McpTransportSpec::SseIde { url, .. }
            | McpTransportSpec::WsIde { url, .. } => url,
            McpTransportSpec::Stdio { .. }
            | McpTransportSpec::InProcess { .. }
            | McpTransportSpec::SdkControl { .. } => return false,
        };
        url.trim().is_empty()
    }

    /// §18 — claude's CONNECT-TIME url re-validation, run in `Ve`/`Ae`
    /// (2.1.251 @182283917 / @182488170) immediately after the
    /// [`Self::is_unconfigured`] gate and BEFORE dialing:
    /// `let C=t.configError; if(!C&&"url"in t) try{new URL(t.url)}
    /// catch{C="'url' is not a valid URL. ..."}`. It fires even when no
    /// loader-time [`Self::config_error`] was ever recorded — the loader
    /// (`mcp/src/json_config.rs`) only stamps `config_error` for a url that
    /// EXPANDED to empty, so a syntactically invalid but non-empty url (a
    /// bare hostname with no scheme, say) reaches this port's connect path
    /// completely unflagged today.
    ///
    /// Returns `None` when a [`Self::config_error`] is already recorded
    /// (that one wins — oracle telemetry tags it `source:"loader"` instead of
    /// this method's implicit `source:"connect"`, `tengu_mcp_server_config_invalid`,
    /// deferred by name), when the transport carries no `url` field, or when
    /// the url is blank ([`Self::is_unconfigured`] owns that case:
    /// `errorCode:"UNCONFIGURED"`, not `"INVALID_CONFIG"`) or parses.
    ///
    /// The oracle's `errorCode` for a `Some` return is `"INVALID_CONFIG"` —
    /// the same code [`Self::config_error`] already produces — so this is
    /// checked immediately after that one, as the third pre-dial gate in
    /// `mcp/src/registry.rs::connect_locked_inner`.
    #[must_use]
    pub fn connect_time_url_error(&self) -> Option<&'static str> {
        if self.config_error.is_some() {
            return None;
        }
        let url = match &self.spec {
            McpTransportSpec::Sse { url, .. }
            | McpTransportSpec::Http { url, .. }
            | McpTransportSpec::WebSocket { url, .. }
            | McpTransportSpec::SseIde { url, .. }
            | McpTransportSpec::WsIde { url, .. } => url,
            McpTransportSpec::Stdio { .. }
            | McpTransportSpec::InProcess { .. }
            | McpTransportSpec::SdkControl { .. } => return None,
        };
        if url.trim().is_empty() {
            return None; // `is_unconfigured`'s case, not this one.
        }
        if url::Url::parse(url).is_err() {
            Some(INVALID_URL_ERROR)
        } else {
            None
        }
    }
}

/// `skip_serializing_if` predicate: omit a `bool` field from the serialized
/// form when it holds its `false` default (keeps the on-wire shape unchanged
/// for the common case).
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// Origin of an [`McpServerConfig`]; drives the approval policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[allow(missing_docs)]
pub enum ConfigScope {
    /// A settings tier's `.mcp.json` / `settings.json` servers.
    Settings(lingxi_core::types::SettingsScope),
    Dynamic,
    /// Enterprise-pushed servers. Kept separate from
    /// `Settings(SettingsScope::Managed)`: this port splits the one
    /// enterprise-managed tier in two, and `config_scope_wire` spells them
    /// `"enterprise"` and `"managed"` distinctly.
    Enterprise,
    ClaudeAi,
    /// Agent frontmatter `mcpServers` (claude scope `"agent"`, stamped by
    /// `agentMcpSpecsToScopedConfigs`). Session-scoped like [`Self::Dynamic`],
    /// but NEVER project-approval-gated (claude's approval prompt covers
    /// `.mcp.json` project servers only) and subject to the enterprise
    /// allow/deny policy at merge time (claude `Z__` contains `"agent"`).
    Agent,
}

/// State machine for one MCP connection.
///
/// All transitions go through [`crate::registry::McpRegistry`]; consumers
/// only read the current state.
#[derive(Debug, Clone)]
pub enum McpConnectionState {
    /// Not yet connected, optionally carrying the most recent error.
    Disconnected {
        /// Config the registry will use on the next connect attempt.
        config: McpServerConfig,
        /// Last error, if any.
        last_error: Option<String>,
    },
    /// Connect call is in-flight.
    Connecting {
        /// Config being connected.
        config: McpServerConfig,
        /// When the attempt started.
        started_at: SystemTime,
    },
    /// Connect handshake is waiting on an OAuth callback.
    AwaitingOAuth {
        /// Config waiting for OAuth completion.
        config: McpServerConfig,
        /// Loopback port we are listening on.
        callback_port: u16,
    },
    /// Active connection with discovered capabilities and tools.
    Connected {
        /// Config for the active connection.
        config: McpServerConfig,
        /// Transport-issued connection identifier.
        connection_id: McpConnectionId,
        /// Server capabilities returned by `initialize`.
        capabilities: ServerCapabilitiesDto,
        /// Protocol family/version negotiated for this live connection.
        negotiated: lingxi_core::host::McpNegotiatedProtocol,
        /// Tools advertised by the server.
        tools: Vec<McpToolDto>,
        /// Resources advertised by the server.
        resources: Vec<McpResourceDto>,
        /// Parameterized resource templates advertised by the server
        /// (`resources/templates/list`, §26a).
        resource_templates: Vec<McpResourceTemplateDto>,
        /// Prompts advertised by the server.
        prompts: Vec<McpPromptDto>,
        /// When the connection became `Connected`.
        connected_at: SystemTime,
    },
    /// §11 Stage 2 — served entirely from the discovery cache: no transport
    /// was ever dialed. A lazily-dialed cached server IS a fresh connection
    /// (the transport was never opened), so this carries a freshly allocated
    /// [`McpConnectionId`] with NOTHING live behind it —
    /// [`crate::raw_conn::RawConnectionProvider::connection_for`] naturally
    /// returns `None` for it (an unrecognized id), and [`McpConnectionId`] is
    /// generated fresh here precisely so `connect()`'s signature and every
    /// caller stay unchanged. The catalog fields mirror [`Self::Connected`]
    /// exactly (same names/types) so match sites can share one arm via an
    /// or-pattern (`Connected {..} | Cached {..}`) wherever the two are
    /// semantically interchangeable — see `mcp::registry`'s Stage 2 doc for
    /// which call sites do and don't get a `Cached` arm.
    ///
    /// Upgraded to a real [`Self::Connected`] on the first tool dispatch
    /// (`mcp::registry::McpRegistry::call_tool_with_auth_retry`'s lazy dial),
    /// which re-runs the ordinary connect path — this is NOT a special
    /// "resume" state; a lazily-dialed cached server is just a fresh connect
    /// that happened to skip the dial once.
    Cached {
        /// Config for the connection a lazy dial will use.
        config: McpServerConfig,
        /// Freshly allocated identifier; no live transport connection is
        /// registered under it until the lazy dial upgrades this state.
        connection_id: McpConnectionId,
        /// Server capabilities from the cached `initialize` round.
        capabilities: ServerCapabilitiesDto,
        /// Protocol family/version recorded when the cached catalog was
        /// populated. No transport is live in this state.
        negotiated: lingxi_core::host::McpNegotiatedProtocol,
        /// Tools from the cached `tools/list` round.
        tools: Vec<McpToolDto>,
        /// Resources from the cached `resources/list` round.
        resources: Vec<McpResourceDto>,
        /// Resource templates from the cached `resources/templates/list` round.
        resource_templates: Vec<McpResourceTemplateDto>,
        /// Prompts from the cached `prompts/list` round.
        prompts: Vec<McpPromptDto>,
        /// The cache entry's own `saved_at_ms` (oracle `cacheSavedAt`).
        cache_saved_at_ms: u64,
        /// Entry age at decision time, ms ([`crate::discovery_cache::Decision`]'s
        /// `age_ms`) — reported on `tengu_mcp_discovery_source`'s `entryAgeMs`.
        age_ms: u64,
    },
    /// A liveness ping is in-flight.
    HealthChecking {
        /// Connection being pinged.
        connection_id: McpConnectionId,
        /// Config of the connection being pinged.
        config: McpServerConfig,
    },
    /// Backoff before the next reconnect attempt.
    Reconnecting {
        /// Config to retry.
        config: McpServerConfig,
        /// Consecutive failure count.
        retry_count: u32,
        /// Earliest time the next attempt may run.
        next_retry_at: SystemTime,
    },
    /// Permanently failed after exhausting retries.
    Failed {
        /// Config that failed.
        config: McpServerConfig,
        /// Most recent error message.
        error: String,
        /// Total attempts made.
        attempts: u32,
    },
    /// Explicitly stopped by the user.
    Stopped {
        /// Config of the stopped connection.
        config: McpServerConfig,
    },
}

impl McpConnectionState {
    /// Logical server name carried by every variant.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Disconnected { config, .. }
            | Self::Connecting { config, .. }
            | Self::AwaitingOAuth { config, .. }
            | Self::Connected { config, .. }
            | Self::Cached { config, .. }
            | Self::HealthChecking { config, .. }
            | Self::Reconnecting { config, .. }
            | Self::Failed { config, .. }
            | Self::Stopped { config } => &config.name,
        }
    }

    /// The originating [`McpServerConfig`], carried by every variant. Used by
    /// [`crate::registry::McpRegistry::reconnect`] to re-establish a connection
    /// after tearing the live one down.
    #[must_use]
    pub fn config(&self) -> &McpServerConfig {
        match self {
            Self::Disconnected { config, .. }
            | Self::Connecting { config, .. }
            | Self::AwaitingOAuth { config, .. }
            | Self::Connected { config, .. }
            | Self::Cached { config, .. }
            | Self::HealthChecking { config, .. }
            | Self::Reconnecting { config, .. }
            | Self::Failed { config, .. }
            | Self::Stopped { config } => config,
        }
    }

    /// Transport-kind label of the connection's config — `"stdio"`,
    /// `"sse"`, `"http"`, etc. Used by [`crate::registry::McpRegistry::snapshot`]
    /// (M6-07) to populate `McpServerInfo::transport`.
    #[must_use]
    pub fn transport_kind(&self) -> &'static str {
        let cfg = match self {
            Self::Disconnected { config, .. }
            | Self::Connecting { config, .. }
            | Self::AwaitingOAuth { config, .. }
            | Self::Connected { config, .. }
            | Self::Cached { config, .. }
            | Self::HealthChecking { config, .. }
            | Self::Reconnecting { config, .. }
            | Self::Failed { config, .. }
            | Self::Stopped { config } => config,
        };
        cfg.spec.kind()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::McpHeaders;

    fn cfg(spec: McpTransportSpec, config_error: Option<&str>) -> McpServerConfig {
        McpServerConfig {
            name: "srv".to_string(),
            spec,
            scope: ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
            disabled: false,
            timeout_ms: None,
            discovery_cache: None,
            always_load: false,
            tools: Vec::new(),
            tool_permissions: BTreeMap::new(),
            config_error: config_error.map(str::to_string),
            metadata: McpServerMetadata::default(),
        }
    }

    fn http_spec(url: &str) -> McpTransportSpec {
        McpTransportSpec::Http {
            url: url.to_string(),
            headers: McpHeaders::default(),
            headers_helper: None,
            oauth: None,
        }
    }

    fn stdio_spec() -> McpTransportSpec {
        McpTransportSpec::Stdio {
            command: "cmd".to_string(),
            args: vec![],
            env: std::collections::HashMap::default(),
        }
    }

    // ── `connect_time_url_error` ──

    #[test]
    fn malformed_nonempty_url_is_flagged_invalid() {
        // A non-empty url with no scheme is exactly the oracle's `try{new
        // URL(t.url)}catch{...}` failure case — `is_unconfigured` (a
        // trim().is_empty() check) does NOT catch it, so without this method
        // it would reach the dial unflagged.
        let c = cfg(http_spec("not-a-url"), None);
        assert!(!c.is_unconfigured(), "non-empty url is not UNCONFIGURED");
        assert_eq!(c.connect_time_url_error(), Some(INVALID_URL_ERROR));
    }

    #[test]
    fn wellformed_url_is_not_flagged() {
        let c = cfg(http_spec("https://mcp.example/api"), None);
        assert_eq!(c.connect_time_url_error(), None);
    }

    #[test]
    fn blank_url_is_unconfigured_not_invalid() {
        // `is_unconfigured` owns the blank-url case (`errorCode:"UNCONFIGURED"`);
        // this method must defer to it, not double-report as INVALID_CONFIG.
        let c = cfg(http_spec(""), None);
        assert!(c.is_unconfigured());
        assert_eq!(c.connect_time_url_error(), None);
    }

    #[test]
    fn existing_loader_config_error_wins_over_the_connect_time_check() {
        // A loader-stamped `config_error` (source:"loader") must not be
        // overridden or duplicated by this connect-time (source:"connect")
        // re-check, even when the url also happens to be unparseable.
        let c = cfg(http_spec("not-a-url"), Some("expanded to an empty string"));
        assert_eq!(c.connect_time_url_error(), None);
    }

    #[test]
    fn transports_without_a_url_field_are_never_flagged() {
        let c = cfg(stdio_spec(), None);
        assert_eq!(c.connect_time_url_error(), None);
    }
}
