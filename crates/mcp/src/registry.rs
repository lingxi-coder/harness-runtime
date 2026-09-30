//! Owns every MCP connection and drives the state machine.
//!
//! Engine code holds an `Arc<McpRegistry>` and uses [`Self::connect`] /
//! [`Self::disconnect`] to manage servers. Startup auto-connect
//! ([`McpRegistry::connect_all`]) and the reconnect/backoff loop
//! ([`McpRegistry::run_reconnect_loop`]) implement the "Plan 13" wiring.

use crate::client::McpClient;
use crate::connection::{McpConnectionState, McpServerConfig};
use crate::hook_dispatch::HookDispatcher;
use crate::normalization::normalize_name_for_mcp;
use crate::oauth::{self, OnAuthorizationUrl};
use crate::raw_conn::RawConnectionProvider;
use futures_util::FutureExt as _;
use indexmap::IndexMap;
use lingxi_core::host::{
    Clock, HttpTransport, McpError, McpNotificationStream, McpRawConnection, McpTransport,
    McpTransportSpec, SecureStorage, ServerCapabilitiesDto,
};
use lingxi_core::types::{AgentId, McpConnectionId};
use std::collections::HashMap;
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime};
use tokio::sync::{broadcast, Mutex, Notify, RwLock};
mod authentication;
mod catalog_refresh;
mod diagnostics;
mod discovery_cache;
mod local_apps;
use authentication::error_is_401;
use authentication::error_is_403_insufficient_scope;
pub(crate) use authentication::error_is_auth_response;
use authentication::error_resource_metadata_url;
use authentication::spec_oauth;
use authentication::spec_url;
use diagnostics::connected_zero_tools_fires;
use diagnostics::degraded_payloads_for_server;
use diagnostics::discovery_source_emission;
use diagnostics::emit_degraded;
use diagnostics::emit_resource_templates_fetched;
use diagnostics::emit_server_config_invalid;
use diagnostics::emit_server_connection_failed;
use diagnostics::emit_server_connection_succeeded;
use diagnostics::emit_server_needs_auth_for_config;
use diagnostics::emit_session_expired;
use diagnostics::emit_tool_call_auth_error_for_config;
use diagnostics::emit_tools_listed;
use diagnostics::resource_templates_fetched_payload;
use diagnostics::sanitize_diagnostic;
#[cfg(test)]
use diagnostics::server_config_invalid_payload;
use diagnostics::server_connection_failed_payload;
use diagnostics::server_connection_succeeded_payload;
use diagnostics::telemetry_mcp_server_base_url;
use diagnostics::tool_call_auth_error_code;
use diagnostics::tools_listed_payload;
#[cfg(test)]
use diagnostics::xaa_flow_failure_stage;

/// OAuth seam injected into the registry for remote (SSE/HTTP) MCP servers that
/// declare an `oauth` config. When unset, OAuth-configured servers fall back to
/// their static headers (no Bearer attach), and static-token servers are
/// entirely unaffected. Mirrors claude-code's `services/mcp/auth.ts` wiring.
#[derive(Clone)]
pub struct OAuthDeps {
    /// HTTP transport used for `.well-known` discovery, DCR, token exchange,
    /// and refresh against the authorization server.
    pub http: Arc<dyn HttpTransport>,
    /// Wall-clock source used to compute / check token expiry.
    pub clock: Arc<dyn Clock>,
    /// Secure storage backing per-server token persistence (`mcp-oauth`
    /// service, account = `oauth::server_key`).
    pub storage: Arc<dyn SecureStorage>,
    /// Host hook invoked with the authorization URL so the TUI / desktop can
    /// open a browser. Fired once per interactive flow.
    pub on_authorization_url: OnAuthorizationUrl,
    /// Optional Cross-App-Access (XAA / SEP-990) config provider. When wired AND
    /// `LINGXI_ENABLE_XAA` is truthy, an `oauth.xaa==Some(true)` server resolves
    /// its token via the RFC 8693 → RFC 7523 token-exchange chain
    /// ([`crate::xaa::perform_cross_app_access`]) instead of the consent flow.
    ///
    /// This provider supplies the per-server IdP + AS inputs (`id_token`, AS
    /// `client_secret`, IdP token endpoint) that claude-code gathers via
    /// `getXaaIdpSettings`/`acquireIdpIdToken`/`discoverOidc`/`mcpOAuthClientConfig`
    /// (auth.ts:676-744). That IdP-login/secret surface has no config seam in
    /// this codebase yet, so when this is `None` an XAA-flagged server hard-fails
    /// with an actionable residual message rather than silently degrading.
    // Dense OAuth/OIDC vocabulary (IdP, OIDC, AS) reads worse backticked.
    #[allow(clippy::doc_markdown)]
    pub xaa_config: Option<Arc<dyn XaaConfigProvider>>,
}

/// Per-server XAA inputs (the IdP `id_token` + AS/IdP credentials) gathered by
/// the host. Mirrors the bundle claude-code's `performMCPXaaAuth` assembles
/// from user settings + keychain before calling `performCrossAppAccess`
/// (auth.ts:676-744). The IdP browser-login that mints `id_token`
/// (`acquireIdpIdToken`) lives behind this seam.
#[allow(clippy::doc_markdown)]
#[derive(Debug, Clone)]
pub struct XaaInputs {
    /// AS-registered confidential client id (`serverConfig.oauth.clientId`).
    pub client_id: String,
    /// AS-registered confidential client secret (`mcpOAuthClientConfig`).
    pub client_secret: String,
    /// IdP-registered client id (`idp.clientId`).
    pub idp_client_id: String,
    /// Optional IdP client secret (`getIdpClientSecret`).
    pub idp_client_secret: Option<String>,
    /// The user's OIDC `id_token` (cached or freshly minted by IdP login).
    pub idp_id_token: String,
    /// IdP token endpoint (`discoverOidc(...).token_endpoint`).
    pub idp_token_endpoint: String,
}

/// Seam supplying [`XaaInputs`] for an XAA-flagged server. Implemented by the
/// host (desktop/CLI) once the IdP-settings + secret config surface exists.
#[async_trait::async_trait]
pub trait XaaConfigProvider: Send + Sync {
    /// Resolve the XAA inputs for `server_name` / `server_url`, performing the
    /// IdP login (or cache hit) as needed. Returns `Ok(None)` when this server
    /// is not actually XAA-provisioned (caller hard-fails with guidance).
    async fn xaa_inputs(
        &self,
        server_name: &str,
        server_url: &str,
    ) -> Result<Option<XaaInputs>, McpError>;

    /// Whether the provider can confirm a cached IdP `id_token` exists before
    /// attempting XAA acquisition. Mirrors claude-code's pre-acquire cache peek
    /// used for `idTokenCacheHit` analytics.
    async fn peek_id_token_cache_hit(
        &self,
        _server_name: &str,
        _server_url: &str,
    ) -> Result<bool, McpError> {
        Ok(false)
    }

    /// Drop the cached IdP `id_token` so the next [`Self::xaa_inputs`] call
    /// re-acquires a fresh one (auth.ts `clearIdpIdToken(idp.issuer)`, 1840-1847).
    ///
    /// Called by [`McpRegistry::resolve_xaa_token`] only when the cross-app
    /// token exchange returns a 4xx (the cached `id_token` was rejected); a 5xx
    /// (IdP outage) keeps it. The default is a no-op so providers that don't
    /// cache an `id_token` need no change.
    async fn clear_id_token(&self) -> Result<(), McpError> {
        Ok(())
    }
}

/// Initial reconnect backoff (claude-code `INITIAL_BACKOFF_MS = 1000`).
const INITIAL_BACKOFF: Duration = Duration::from_millis(1000);
/// Ceiling on reconnect backoff (claude-code `MAX_BACKOFF_MS = 30000`).
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Host-owned, provider-neutral guard used by asynchronous reconciliation.
///
/// The callback is intentionally synchronous: registry operations invoke it
/// only after acquiring the per-server lifecycle lock, immediately before a
/// state mutation. This lets a host invalidate an in-flight operation without
/// exposing registry internals or retaining a transport handle.
pub type McpOperationGuard = dyn Fn() -> bool + Send + Sync;

const OPERATION_GUARD_REJECTED: &str = "MCP operation superseded";

fn operation_guard_rejected() -> McpError {
    McpError::Internal(OPERATION_GUARD_REJECTED.to_string())
}

fn is_operation_guard_rejected(error: &McpError) -> bool {
    matches!(error, McpError::Internal(message) if message == OPERATION_GUARD_REJECTED)
}

/// MCP server catalog affected by an inbound `notifications/*/list_changed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpCatalogKind {
    /// The server's `tools/list` result changed.
    Tools,
    /// The server's `prompts/list` result changed.
    Prompts,
    /// The server's `resources/list` result changed.
    Resources,
}

/// One list-changed notification associated with the connection that emitted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCatalogChanged {
    /// Raw configured server name.
    pub server_name: String,
    /// Connection generation that emitted the notification.
    pub connection_id: McpConnectionId,
    /// Previous connection partition to remove before applying this change.
    /// Set on disconnect; ordinary list invalidations and fresh connections
    /// leave it `None`.
    pub retired_connection_id: Option<McpConnectionId>,
    /// Catalog to refresh.
    pub kind: McpCatalogKind,
    /// Present only when this refresh came from a real inbound
    /// `notifications/*/list_changed` producer whose telemetry should be
    /// emitted after a successful re-fetch. Recovery snapshots, connect
    /// publishes, and retire notifications leave this `None`.
    pub telemetry_cause: Option<&'static str>,
}

/// Scope for a Local App conversation-export connection. The scope is bound
/// when the Host creates the connection; it is never taken from a tool input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationExport {
    /// Stable Local App identity.
    pub app_id: String,
    /// Digest of the tool surface last exposed to the conversation.
    pub listed_tool_surface_sha256: String,
}

impl ConversationExport {
    /// Validate the schema-v3 App ID and the connection's last-listed surface.
    pub fn new(
        app_id: impl Into<String>,
        listed_tool_surface_sha256: impl Into<String>,
    ) -> Result<Self, McpError> {
        let app_id = app_id.into();
        let digest = listed_tool_surface_sha256.into();
        if !is_local_app_id(&app_id) {
            return Err(McpError::Internal("invalid Local App identity".into()));
        }
        if !is_sha256(&digest) {
            return Err(McpError::Internal(
                "invalid Local App tool surface identity".into(),
            ));
        }
        Ok(Self {
            app_id,
            listed_tool_surface_sha256: digest,
        })
    }

    /// Logical MCP server name for this app.
    #[must_use]
    pub fn server_name(&self) -> String {
        format!("local_app_{}", self.app_id)
    }

    /// Registry key for this logical server.
    #[must_use]
    pub fn registry_key(&self) -> String {
        format!("local_apps:conversation-export:{}", self.app_id)
    }

    /// Build the transport registry key for one conversation-scoped export.
    pub fn scoped_registry_key(&self, conversation_id: &str) -> Result<String, McpError> {
        if !is_conversation_scope_id(conversation_id) {
            return Err(McpError::Internal(
                "invalid Local App conversation scope".into(),
            ));
        }
        Ok(format!(
            "local_apps:conversation-export:{conversation_id}:{}:{}",
            self.app_id, self.listed_tool_surface_sha256
        ))
    }

    /// Parse a conversation-scoped Local App transport registry key.
    pub fn parse_scoped_registry_key(
        key: &str,
    ) -> Result<Option<(String, ConversationExport)>, McpError> {
        let Some(rest) = key.strip_prefix("local_apps:conversation-export:") else {
            return Ok(None);
        };
        let mut parts = rest.splitn(3, ':');
        let (Some(conversation_id), Some(app_id), Some(surface)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Ok(None);
        };
        if !is_conversation_scope_id(conversation_id) {
            return Err(McpError::Internal(
                "invalid Local App conversation scope".into(),
            ));
        }
        Ok(Some((
            conversation_id.to_string(),
            Self::new(app_id.to_string(), surface.to_string())?,
        )))
    }

    /// Stable wire identity shared by every logical Local App server.
    #[must_use]
    pub const fn server_info_name(&self) -> &'static str {
        "lingxi-local-app"
    }

    /// Build and validate one model-facing tool name.
    pub fn tool_full_name(&self, tool_name: &str) -> Result<String, McpError> {
        if tool_name.is_empty()
            || tool_name.len() > 64
            || !tool_name.bytes().enumerate().all(|(index, byte)| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || (byte == b'_' && index > 0)
            })
            || tool_name.starts_with('_')
            || tool_name.ends_with('_')
            || tool_name.contains("__")
        {
            return Err(McpError::ToolNotFound(tool_name.into()));
        }
        Ok(format!("mcp__{}__{}", self.server_name(), tool_name))
    }
}

fn is_local_app_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 54
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn is_conversation_scope_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn is_local_app_tool_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('_')
        && !value.ends_with('_')
        && !value.contains("__")
        && value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || (byte == b'_' && index > 0)
        })
}

/// Host-managed logical Local App server metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedLocalAppServer {
    /// Conversation-export scope.
    pub scope: ConversationExport,
    /// Digest of the currently active catalog.
    pub catalog_sha256: String,
    /// Generation of the exposed tool surface.
    pub surface_generation: u64,
}

/// Optional widget resource exposed by one managed Local App server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedLocalAppResource {
    /// Concrete resource URI advertised to the model.
    pub uri: String,
    /// Human-readable name.
    pub name: String,
    /// Optional description.
    pub description: Option<String>,
    /// Optional MIME type.
    pub mime_type: Option<String>,
    /// MCP Apps resource metadata, including CSP/domain hints.
    pub meta: Option<serde_json::Value>,
}

/// Host-owned runtime overlay for one managed Local App server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedLocalAppRuntime {
    /// Whether the logical server is currently visible/callable.
    pub enabled: bool,
    /// Optional allowlist of raw tool names from the active catalog.
    pub enabled_tools: Option<Vec<String>>,
    /// Optional widget resource advertised through `resources/list`.
    pub resource: Option<ManagedLocalAppResource>,
    /// Monotonic generation for resource metadata changes.
    pub resource_generation: u64,
}

impl Default for ManagedLocalAppRuntime {
    fn default() -> Self {
        Self {
            enabled: true,
            enabled_tools: None,
            resource: None,
            resource_generation: 0,
        }
    }
}

/// One lazily exposed Local App in a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAppExposure {
    /// Stable Local App identity.
    pub app_id: String,
    /// Whether the conversation has explicitly pinned the app.
    pub pinned: bool,
    /// Number of calls currently in flight.
    pub in_flight: usize,
    /// Monotonic recency sequence.
    pub last_used: u64,
    /// Generation of the exposure metadata.
    pub exposure_generation: u64,
}

/// Result of exposing one logical Local App server in a conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAppExposureUpdate {
    /// The new or refreshed exposure entry.
    pub exposure: LocalAppExposure,
    /// Unpinned idle app evicted by the bounded LRU policy, if any.
    pub evicted_app_id: Option<String>,
}

#[derive(Debug, Default)]
struct ConversationExposureState {
    entries: HashMap<String, LocalAppExposure>,
    next_sequence: u64,
    next_generation: u64,
}

const LOCAL_APP_MAX_EXPOSED: usize = 8;
const LOCAL_APP_MAX_IN_FLIGHT_PER_APP: usize = 4;
const LOCAL_APP_MAX_IN_FLIGHT_PER_CONVERSATION: usize = 8;

struct RegisteredClient {
    connection_id: Option<McpConnectionId>,
    client: Arc<McpClient>,
}

/// Immutable identity of the grant that supplied a live connection's bearer.
/// The value is a provider-neutral hash of the MCP refresh grant, never the
/// access/refresh secret itself. `verify_current` is false for static bearer
/// and non-OAuth connections, where secure storage cannot prove provenance.
#[derive(Clone, Debug, PartialEq, Eq)]
struct GrantProvenance {
    fingerprint: String,
    verify_current: bool,
}

impl GrantProvenance {
    fn unbound() -> Self {
        Self {
            fingerprint: crate::discovery_cache::fingerprint("grant:none"),
            verify_current: false,
        }
    }

    fn from_grant_token(grant_token: &str, verify_current: bool) -> Self {
        Self {
            fingerprint: crate::discovery_cache::fingerprint(grant_token),
            verify_current,
        }
    }

    fn from_tokens(tokens: &oauth::Tokens) -> Option<Self> {
        let refresh_token = tokens
            .refresh_token
            .as_ref()
            .map(|token| token.expose_secret())
            .filter(|token| !token.is_empty())?;
        let grant_token = oauth::discovery_cache_refresh_grant_token(refresh_token);
        Some(Self::from_grant_token(&grant_token, true))
    }
}

#[derive(Clone)]
enum LazyUpgradeTerminal {
    Success(McpConnectionId),
    Error(Arc<McpError>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LazyUpgradeMode {
    Foreground,
    Background,
}

struct LazyUpgradeSlot {
    key: String,
    cached_connection_id: McpConnectionId,
    expected_config: McpServerConfig,
    refresh_partition: Option<DiscoveryCachePartition>,
    /// Actual protocol era recorded by the stale entry. This is distinct
    /// from the partition's expected era: auto negotiation can select the
    /// modern partition and still fall back to a legacy live handshake.
    refresh_entry_era: Option<String>,
    /// The immutable resolver result captured when this lazy dial started.
    negotiation_mode: crate::protocol_negotiation::NegotiationMode,
    mode: LazyUpgradeMode,
    terminal: StdMutex<Option<LazyUpgradeTerminal>>,
    notify: Notify,
}

impl LazyUpgradeSlot {
    fn new(
        key: String,
        cached_connection_id: McpConnectionId,
        expected_config: McpServerConfig,
        refresh_partition: Option<DiscoveryCachePartition>,
        refresh_entry_era: Option<String>,
        negotiation_mode: crate::protocol_negotiation::NegotiationMode,
        mode: LazyUpgradeMode,
    ) -> Self {
        Self {
            key,
            cached_connection_id,
            expected_config,
            refresh_partition,
            refresh_entry_era,
            negotiation_mode,
            mode,
            terminal: StdMutex::new(None),
            notify: Notify::new(),
        }
    }

    fn matches(&self, cached_connection_id: McpConnectionId, config: &McpServerConfig) -> bool {
        self.cached_connection_id == cached_connection_id
            && McpRegistry::same_config_snapshot(&self.expected_config, config)
    }

    fn stale_error(&self) -> McpError {
        McpError::Connection(format!("MCP server \"{}\" is no longer cached", self.key))
    }

    fn panic_error(&self) -> McpError {
        McpError::Internal(format!(
            "MCP server \"{}\" cached lazy-upgrade task panicked",
            self.key
        ))
    }

    fn finish_if_unset(&self, terminal: LazyUpgradeTerminal) -> bool {
        let mut guard = self
            .terminal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_some() {
            return false;
        }
        *guard = Some(terminal);
        drop(guard);
        self.notify.notify_waiters();
        true
    }

    async fn wait(&self) -> Result<McpConnectionId, McpError> {
        loop {
            let notified = self.notify.notified();
            let terminal = {
                self.terminal
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            };
            if let Some(terminal) = terminal {
                return match terminal {
                    LazyUpgradeTerminal::Success(connection_id) => Ok(connection_id),
                    LazyUpgradeTerminal::Error(error) => Err(clone_mcp_error(error.as_ref())),
                };
            }
            notified.await;
        }
    }
}

#[derive(Clone)]
struct LiveDiscovery {
    connection_id: McpConnectionId,
    connection_duration_ms: u64,
    /// Immutable resolver result used for this entire connect attempt.
    negotiation_mode: crate::protocol_negotiation::NegotiationMode,
    /// Immutable grant identity captured alongside the successful connect
    /// spec; write-through revalidates it before persisting any catalog.
    grant_provenance: Option<GrantProvenance>,
    negotiated: lingxi_core::host::McpNegotiatedProtocol,
    capabilities: ServerCapabilitiesDto,
    tools: Vec<lingxi_core::host::McpToolDto>,
    resources: Vec<lingxi_core::host::McpResourceDto>,
    resource_templates: Vec<lingxi_core::host::McpResourceTemplateDto>,
    prompts: Vec<lingxi_core::host::McpPromptDto>,
    catalog_failures: CatalogFetchFailures,
    discovery_cache_partition: Option<DiscoveryCachePartition>,
    client: Option<Arc<McpClient>>,
    listener_connection: Option<Arc<jsonrpc::Connection>>,
}

#[derive(Clone, Copy, Default)]
struct CatalogFetchFailures {
    tools: bool,
    resources: bool,
    prompts: bool,
}

enum BackgroundInstallOutcome {
    Installed(McpConnectionId),
    Rejected(LiveDiscovery),
}

#[derive(Debug, Clone)]
struct PendingTransportCleanup {
    retrying: bool,
}

#[derive(Clone, Default)]
struct ListenerReopenState {
    delay_index: usize,
    opened_at: Option<tokio::time::Instant>,
    reopened_at: Vec<tokio::time::Instant>,
}

#[derive(Clone, Copy)]
struct ModernListenOpenTelemetry {
    outcome: telemetry::tengu::mcp::ListenReopenOutcome,
    attempts: u32,
    trigger: telemetry::tengu::mcp::ListenReopenTrigger,
}

#[derive(Clone)]
struct PromptPredecessor {
    key: String,
    config: McpServerConfig,
    live_connection_id: McpConnectionId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DiscoveryCachePartition {
    logical_key: String,
    partition_key: String,
    expected_era: &'static str,
    /// The resolver decision that produced this partition. Keeping the
    /// budget here prevents a later re-resolution from changing the probe
    /// deadline or expected era during write-through/revalidation.
    negotiation_mode: crate::protocol_negotiation::NegotiationMode,
}

struct DiscoveryCacheConsult {
    decision: crate::discovery_cache::Decision,
    partition: Option<DiscoveryCachePartition>,
}

enum LazyUpgradePreparation {
    Connected(McpConnectionId),
    Wait(Arc<LazyUpgradeSlot>, bool),
    Skip,
}

#[cfg(test)]
#[derive(Default)]
struct TestPauseHook {
    entered: Notify,
    release: Notify,
}

/// In-memory registry of every known MCP connection.
pub struct McpRegistry {
    /// Map of server name to current state.
    ///
    /// `pub` so the CLI binary (M6-07 init.rs) can pre-populate
    /// `Disconnected` entries read from `.mcp.json` before the engine
    /// connects, and so engine-side tests can seed states directly.
    pub connections: Arc<RwLock<HashMap<String, McpConnectionState>>>,
    /// Host-managed Local App logical servers. This is metadata only; all
    /// entries share the registry's physical transport substrate.
    managed_local_apps: Arc<RwLock<HashMap<String, ManagedLocalAppServer>>>,
    /// Host-managed runtime overlays for Local App logical servers.
    managed_local_app_runtime: Arc<RwLock<HashMap<String, ManagedLocalAppRuntime>>>,
    /// Per-conversation bounded, lazy Local App exposure state.
    local_app_exposures: Arc<RwLock<HashMap<String, ConversationExposureState>>>,
    /// Synchronous mirror of claude-code 2.1.238's `eZf()`
    /// (`bdl(b7e()??[]).length>0`, `cc-238.js @229641619`) — "at least one MCP
    /// client is `type === "pending"`".
    ///
    /// [`Self::connections`] lives behind an async `RwLock`, but the predicate
    /// is needed from `Tool::is_enabled`, which is synchronous. Callers that can
    /// `await` refresh it with [`Self::refresh_pending_servers`] before building
    /// a tool list; [`Self::has_pending_servers`] then reads it without
    /// blocking. Starts `false`, so a host that never refreshes behaves exactly
    /// as it did before this mirror existed.
    pending_servers: Arc<std::sync::atomic::AtomicBool>,
    /// Serializes connect/disconnect/reconnect for each logical server without
    /// holding the public connection-state lock across transport or OAuth I/O.
    /// Different servers still progress independently.
    lifecycle_locks: Arc<StdMutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Single-flight guard for the XAA token resolve chain, keyed by
    /// `oauth::server_key`. Mirrors the oracle's `_refreshInProgress`
    /// promise-sharing guard on `tokens()`/`xaaRefresh()` (@182213696, §26b
    /// delta 2): concurrent resolves for the SAME server share one exchange —
    /// a resolver that has to wait re-reads storage once it acquires the lock
    /// and reuses whatever the winner just persisted instead of re-exchanging.
    xaa_refresh_locks: Arc<StdMutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Side-channel cache of [`McpClient`] handles per server name.
    ///
    /// Populated by [`Self::register_client`] and live connection install paths.
    /// Production wiring records the exact connection generation alongside the
    /// client so prompt dispatch can fail closed when a reconnect swaps the
    /// live client between command discovery and `prompts/get`.
    ///
    /// Insertion-ordered ([`IndexMap`]) so [`Self::servers_with_tools`] returns
    /// server names in DISCOVERY order — claude builds `serversWithTools` by
    /// iterating `appState.mcp.tools` in order with no sort
    /// (`AgentTool.tsx:394-405`). A plain `HashMap` would make the required-MCP
    /// gate error text non-deterministic.
    clients: Arc<RwLock<IndexMap<String, RegisteredClient>>>,
    /// Fan-out for inbound server catalog invalidations. The engine subscribes
    /// once and refreshes the shared tool registry after a successful
    /// `tools/list`; lagged consumers reconcile against the latest shared active
    /// generations instead of replaying permanent tombstones.
    catalog_changes: broadcast::Sender<McpCatalogChanged>,
    /// Per-agent connection scoping (subagent isolation).
    #[allow(dead_code)] // populated by `register_for_agent` in Plan 13
    agent_scoped: Arc<RwLock<HashMap<AgentId, HashMap<String, McpConnectionId>>>>,
    /// Transport boundary supplied by the host platform.
    transport: Arc<dyn McpTransport>,
    /// Optional bridge to the transport's live `jsonrpc::Connection`s.
    ///
    /// When `Some`, [`Self::connect`] builds an [`McpClient`] over the
    /// transport-owned connection and caches it via [`Self::register_client`],
    /// so the 4 builtin MCP tools reach the server at runtime (via
    /// [`Self::get_client`]). When `None` (the `new` path), no client is
    /// registered and `get_client` keeps returning whatever was seeded
    /// manually (e.g. `register_test_client`). Mirrors claude-code's
    /// `ensureConnectedClient` returning a live client (client.ts:1688-1709).
    raw_conn: Option<Arc<dyn RawConnectionProvider>>,
    /// Optional hook-dispatch seam forwarded into each [`McpClient`]'s
    /// `elicitation/create` handler. When `Some`, an incoming elicitation
    /// consults the engine's `Elicitation` hook (claude-code
    /// `runElicitationHooks`); when `None` (the default) the handler keeps its
    /// `{"action":"cancel"}` behavior. Set via [`Self::with_hook_dispatcher`],
    /// matching the `RawConnectionProvider` injection pattern.
    hook_dispatcher: Option<Arc<dyn HookDispatcher>>,
    /// Optional OAuth 2.1 + PKCE seam for remote MCP servers configured with
    /// an `oauth` block. When `None` (the default), [`Self::connect`] never
    /// runs the OAuth flow and OAuth-configured servers connect with only their
    /// static headers; static-token servers are unaffected either way. Wired
    /// via [`Self::with_oauth`].
    oauth: Option<OAuthDeps>,
    /// Session cwd used by dynamic MCP header helpers.
    headers_helper_cwd: std::path::PathBuf,
    /// Plugin roots keyed by scoped MCP server name.
    headers_helper_plugin_roots: Arc<RwLock<HashMap<String, std::path::PathBuf>>>,
    /// LIVE additional working directories (settings `additionalDirectories`
    /// union CLI `--add-dir`, plus any runtime `/add-dir`) advertised alongside
    /// cwd on each server's `roots/list`.
    ///
    /// A SHARED [`crate::SharedRoots`] cell — the SAME `Arc` is forwarded into
    /// every [`McpClient`] built by [`Self::connect`] (via
    /// [`McpClient::with_roots`]), so a directory pushed via [`Self::add_root`]
    /// at runtime is seen by ALL connected servers' `roots/list` handlers
    /// without a reconnect, matching claude-code 2.1.207 `r1d()`
    /// (`[cwd, ...additionalWorkingDirectories]`). Empty by default (cwd-only
    /// roots, unchanged). Set via [`Self::with_additional_roots`].
    additional_roots: crate::SharedRoots,
    /// Interval used by the background health-check task.
    #[allow(dead_code)] // consumed by the health-check loop in Plan 13
    pub health_check_interval: Duration,
    /// Maximum consecutive reconnect attempts before declaring `Failed`.
    ///
    /// Consumed by [`Self::run_reconnect_loop`]; matches claude-code's
    /// `MAX_RECONNECT_ATTEMPTS = 5`.
    pub max_retry_count: u32,
    /// §11 — the discovery-cache store. `None` by default (every existing
    /// caller unaffected): no entry is ever written, no decision is ever
    /// consulted, no `tengu_mcp_discovery_source` telemetry fires. Set via
    /// [`Self::with_discovery_cache_store`]. The desktop composition root
    /// supplies `<lingxi_home>/mcp-discovery-cache`; this registry only knows
    /// how to read/write the [`crate::discovery_cache::DiscoveryCacheStore`]
    /// it is handed.
    discovery_cache_store: Option<Arc<crate::discovery_cache::DiscoveryCacheStore>>,
    /// Best-effort cleanup retries for transport ids that MUST be disconnected
    /// eventually (for example a background-revalidation CAS reject) even when
    /// the first `disconnect` attempt fails. Retries are bounded; a permanent
    /// failure leaves an observable pending entry that later lifecycle activity
    /// can kick again.
    pending_transport_cleanups: Arc<RwLock<HashMap<McpConnectionId, PendingTransportCleanup>>>,
    /// Modern `subscriptions/listen` reopen bookkeeping keyed by shared
    /// server name.
    listener_reopen_state: Arc<RwLock<HashMap<String, ListenerReopenState>>>,
    /// Detached single-flight owners for cached->live upgrades. Waiters hold a
    /// cloned slot handle, so removing the map entry invalidates future joins
    /// without racing already-waiting callers.
    lazy_upgrade_slots: Arc<RwLock<HashMap<String, Arc<LazyUpgradeSlot>>>>,
    /// Exact cached-prompt generation bridges (`C -> L1`). Kept only while the
    /// current state+client still publish that first live generation.
    prompt_predecessors: Arc<RwLock<HashMap<McpConnectionId, PromptPredecessor>>>,
    /// Builder-time connection semantics are immutable after the first
    /// connect/connect-agent-scoped attempt so later `with_*` mutations cannot
    /// fork behavior away from already-published connections.
    configuration_frozen: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    pause_after_initial_client_miss: Option<Arc<TestPauseHook>>,
    #[cfg(test)]
    pause_before_client_publish: Option<Arc<TestPauseHook>>,
}

/// Message for a remote server with no usable URL (oracle
/// `"No URL configured for this server"`).
pub const UNCONFIGURED_MESSAGE: &str = "No URL configured for this server";
const LISTEN_REOPEN_CAUSE: &str = "listen_reopen";
const LISTENER_REOPEN_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
const LISTENER_REOPEN_STABLE_RESET: Duration = Duration::from_secs(10);
const LISTENER_REOPEN_GRACEFUL_DELAY: Duration = Duration::from_secs(5);
const LISTENER_REOPEN_WINDOW: Duration = Duration::from_secs(60 * 60);
const LISTENER_REOPEN_PARK: Duration = Duration::from_secs(6 * 60 * 60);
const LISTENER_REOPEN_MAX_ATTEMPTS_PER_WINDOW: usize = 5;
const LISTENER_REOPEN_PARK_POLL: Duration = Duration::from_secs(5);

fn negotiated_protocol_from_cache_entry(
    entry: &crate::discovery_cache::DiscoveryCacheEntry,
) -> lingxi_core::host::McpNegotiatedProtocol {
    let era = match entry.negotiated_era.as_deref() {
        Some("modern") => lingxi_core::host::McpProtocolEra::Modern,
        _ => lingxi_core::host::McpProtocolEra::Legacy,
    };
    lingxi_core::host::McpNegotiatedProtocol {
        era,
        version: match era {
            lingxi_core::host::McpProtocolEra::Modern => "2026-07-28",
            lingxi_core::host::McpProtocolEra::Legacy => "2025-11-25",
        }
        .to_string(),
    }
}

fn negotiated_era_label(era: lingxi_core::host::McpProtocolEra) -> &'static str {
    match era {
        lingxi_core::host::McpProtocolEra::Modern => "modern",
        lingxi_core::host::McpProtocolEra::Legacy => "legacy",
    }
}

fn clone_mcp_error(error: &McpError) -> McpError {
    match error {
        McpError::UnsupportedTransport(kind) => McpError::UnsupportedTransport(*kind),
        McpError::Connection(message) => McpError::Connection(message.clone()),
        McpError::Handshake(message) => McpError::Handshake(message.clone()),
        McpError::HttpResponse {
            status,
            www_authenticate,
        } => McpError::HttpResponse {
            status: *status,
            www_authenticate: www_authenticate.clone(),
        },
        McpError::OAuth(message) => McpError::OAuth(message.clone()),
        McpError::ToolNotFound(message) => McpError::ToolNotFound(message.clone()),
        McpError::Timeout { server, tool, secs } => McpError::Timeout {
            server: server.clone(),
            tool: tool.clone(),
            secs: *secs,
        },
        McpError::Internal(message) => McpError::Internal(message.clone()),
    }
}

fn lazy_upgrade_panic_error(server_name: &str, phase: &str) -> McpError {
    McpError::Internal(format!(
        "MCP server \"{server_name}\" panicked during {phase}"
    ))
}

fn panic_payload_mentions_connect(payload: &(dyn std::any::Any + Send)) -> bool {
    payload
        .downcast_ref::<&str>()
        .is_some_and(|message| message.to_ascii_lowercase().contains("connect"))
        || payload
            .downcast_ref::<String>()
            .is_some_and(|message| message.to_ascii_lowercase().contains("connect"))
}

/// Is this a remote (url-bearing) spec whose URL is blank?
///
/// Oracle `zar(e)`'s fallback arm: `!e.configError && "url" in e &&
/// e.url.trim() === ""`. Stdio servers have no url and are never unconfigured
/// by this test.
#[must_use]
pub fn is_unconfigured_remote(spec: &McpTransportSpec) -> bool {
    match spec {
        McpTransportSpec::Sse { url, .. }
        | McpTransportSpec::Http { url, .. }
        | McpTransportSpec::WebSocket { url, .. }
        | McpTransportSpec::SseIde { url, .. }
        | McpTransportSpec::WsIde { url, .. } => url.trim().is_empty(),
        _ => false,
    }
}

impl McpRegistry {
    fn clone_for_background(&self) -> Self {
        Self {
            connections: Arc::clone(&self.connections),
            managed_local_apps: Arc::clone(&self.managed_local_apps),
            managed_local_app_runtime: Arc::clone(&self.managed_local_app_runtime),
            local_app_exposures: Arc::clone(&self.local_app_exposures),
            pending_servers: Arc::clone(&self.pending_servers),
            lifecycle_locks: Arc::clone(&self.lifecycle_locks),
            xaa_refresh_locks: Arc::clone(&self.xaa_refresh_locks),
            clients: Arc::clone(&self.clients),
            catalog_changes: self.catalog_changes.clone(),
            agent_scoped: Arc::clone(&self.agent_scoped),
            transport: Arc::clone(&self.transport),
            raw_conn: self.raw_conn.clone(),
            hook_dispatcher: self.hook_dispatcher.clone(),
            oauth: self.oauth.clone(),
            headers_helper_cwd: self.headers_helper_cwd.clone(),
            headers_helper_plugin_roots: Arc::clone(&self.headers_helper_plugin_roots),
            additional_roots: self.additional_roots.clone(),
            health_check_interval: self.health_check_interval,
            max_retry_count: self.max_retry_count,
            discovery_cache_store: self.discovery_cache_store.clone(),
            pending_transport_cleanups: Arc::clone(&self.pending_transport_cleanups),
            listener_reopen_state: Arc::clone(&self.listener_reopen_state),
            lazy_upgrade_slots: Arc::clone(&self.lazy_upgrade_slots),
            prompt_predecessors: Arc::clone(&self.prompt_predecessors),
            configuration_frozen: Arc::clone(&self.configuration_frozen),
            #[cfg(test)]
            pause_after_initial_client_miss: self.pause_after_initial_client_miss.clone(),
            #[cfg(test)]
            pause_before_client_publish: self.pause_before_client_publish.clone(),
        }
    }

    /// Build a registry bound to a platform transport.
    ///
    /// No `RawConnectionProvider` is wired, so [`Self::connect`] does NOT build
    /// a live [`McpClient`] — use [`Self::with_raw_conn`] for that.
    #[must_use]
    pub fn new(transport: Arc<dyn McpTransport>) -> Self {
        let (catalog_changes, _unused_rx) = broadcast::channel(64);
        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
            managed_local_apps: Arc::new(RwLock::new(HashMap::new())),
            managed_local_app_runtime: Arc::new(RwLock::new(HashMap::new())),
            local_app_exposures: Arc::new(RwLock::new(HashMap::new())),
            pending_servers: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            lifecycle_locks: Arc::new(StdMutex::new(HashMap::new())),
            xaa_refresh_locks: Arc::new(StdMutex::new(HashMap::new())),
            clients: Arc::new(RwLock::new(IndexMap::new())),
            catalog_changes,
            agent_scoped: Arc::new(RwLock::new(HashMap::new())),
            transport,
            raw_conn: None,
            hook_dispatcher: None,
            oauth: None,
            headers_helper_cwd: std::env::current_dir()
                .unwrap_or_else(|_| std::path::PathBuf::from(".")),
            headers_helper_plugin_roots: Arc::new(RwLock::new(HashMap::new())),
            additional_roots: crate::new_shared_roots(Vec::new()),
            health_check_interval: Duration::from_secs(30),
            max_retry_count: 5,
            discovery_cache_store: None,
            pending_transport_cleanups: Arc::new(RwLock::new(HashMap::new())),
            listener_reopen_state: Arc::new(RwLock::new(HashMap::new())),
            lazy_upgrade_slots: Arc::new(RwLock::new(HashMap::new())),
            prompt_predecessors: Arc::new(RwLock::new(HashMap::new())),
            configuration_frozen: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(test)]
            pause_after_initial_client_miss: None,
            #[cfg(test)]
            pause_before_client_publish: None,
        }
    }

    #[cfg(test)]
    fn with_pause_after_initial_client_miss(mut self, hook: Arc<TestPauseHook>) -> Self {
        self.pause_after_initial_client_miss = Some(hook);
        self
    }

    #[cfg(test)]
    fn with_pause_before_client_publish(mut self, hook: Arc<TestPauseHook>) -> Self {
        self.pause_before_client_publish = Some(hook);
        self
    }

    #[cfg(test)]
    async fn maybe_pause_after_initial_client_miss(&self) {
        if let Some(hook) = &self.pause_after_initial_client_miss {
            hook.entered.notify_one();
            hook.release.notified().await;
        }
    }

    #[cfg(test)]
    async fn maybe_pause_before_client_publish(&self) {
        if let Some(hook) = &self.pause_before_client_publish {
            hook.entered.notify_one();
            hook.release.notified().await;
        }
    }

    /// Wire in a §11 discovery-cache store. See
    /// [`Self::discovery_cache_store`]'s doc.
    #[must_use]
    pub fn with_discovery_cache_store(
        mut self,
        store: crate::discovery_cache::DiscoveryCacheStore,
    ) -> Self {
        self.assert_configuration_mutable("with_discovery_cache_store");
        self.discovery_cache_store = Some(Arc::new(store));
        self
    }

    fn freeze_configuration(&self) {
        self.configuration_frozen
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn assert_configuration_mutable(&self, method: &str) {
        assert!(
            !self
                .configuration_frozen
                .load(std::sync::atomic::Ordering::SeqCst),
            "McpRegistry::{method} cannot be called after the first connect attempt"
        );
    }

    fn lifecycle_lock(&self, name: &str) -> Arc<Mutex<()>> {
        let mut locks = self
            .lifecycle_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            locks
                .entry(name.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// Per-server-key single-flight lock for the XAA resolve chain (§26b
    /// delta 2). See [`Self::xaa_refresh_locks`].
    fn xaa_refresh_lock(&self, key: &str) -> Arc<Mutex<()>> {
        let mut locks = self
            .xaa_refresh_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            locks
                .entry(key.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    async fn publish_catalog_change(&self, change: McpCatalogChanged) {
        let _ = self.catalog_changes.send(change);
    }

    /// Subscribe to inbound MCP catalog invalidations.
    #[must_use]
    pub fn subscribe_catalog_changes(&self) -> broadcast::Receiver<McpCatalogChanged> {
        self.catalog_changes.subscribe()
    }

    /// Subscribe to notifications from the currently live connection for
    /// `server_name`.
    ///
    /// A client-backed subscription is preferred because it is tied directly
    /// to the registry's live JSON-RPC connection and does not expose the
    /// private raw connection handle. Transports without a client bridge can
    /// still provide their own multiplexed notification stream through the
    /// platform seam. Callers should treat an error as "notifications are not
    /// available" and use their documented reconciliation fallback.
    pub async fn subscribe_notifications(
        &self,
        server_name: &str,
    ) -> Result<McpNotificationStream, McpError> {
        if let Some(client) = self.get_client(server_name).await {
            return Ok(client.subscribe_notifications());
        }

        let connection_id = {
            let connections = self.connections.read().await;
            let normalized = normalize_name_for_mcp(server_name);
            connections
                .iter()
                .find(|(name, state)| {
                    normalize_name_for_mcp(name) == normalized
                        && matches!(state, McpConnectionState::Connected { .. })
                })
                .and_then(|(_, state)| match state {
                    McpConnectionState::Connected { connection_id, .. } => Some(*connection_id),
                    _ => None,
                })
        };
        let Some(connection_id) = connection_id else {
            return Err(McpError::Connection(format!(
                "MCP server \"{server_name}\" has no live notification connection"
            )));
        };

        // A platform transport may not implement the optional notification
        // seam yet. Keep that failure local to the caller (usually a monitor)
        // and avoid allowing an implementation panic to take down the task.
        std::panic::AssertUnwindSafe(
            self.transport
                .notifications(&McpRawConnection { connection_id }),
        )
        .catch_unwind()
        .await
        .map_err(|_| {
            McpError::Internal(format!(
                "MCP transport panicked while subscribing to notifications for \"{server_name}\""
            ))
        })?
    }

    /// Build a registry bound to a platform transport AND a bridge to its live
    /// `jsonrpc::Connection`s (typically the same platform object implementing
    /// both [`McpTransport`] and [`RawConnectionProvider`]).
    ///
    /// With `raw_conn` wired, [`Self::connect`] builds an [`McpClient`] per
    /// connected server and caches it so [`Self::get_client`] returns a working
    /// client to the builtin MCP tools.
    #[must_use]
    pub fn with_raw_conn(
        transport: Arc<dyn McpTransport>,
        raw_conn: Arc<dyn RawConnectionProvider>,
    ) -> Self {
        Self {
            raw_conn: Some(raw_conn),
            ..Self::new(transport)
        }
    }

    /// Inject the optional [`HookDispatcher`] forwarded into every
    /// [`McpClient`] built by [`Self::connect`]. Builder-style so it composes
    /// with [`Self::new`] / [`Self::with_raw_conn`]:
    ///
    /// ```ignore
    /// let reg = McpRegistry::with_raw_conn(transport, raw_conn)
    ///     .with_hook_dispatcher(Some(orchestrator_dispatcher));
    /// ```
    ///
    /// `None` leaves the default behavior (handler returns
    /// `{"action":"cancel"}`); `Some(_)` enables the `Elicitation` hook path.
    #[must_use]
    pub fn with_hook_dispatcher(mut self, dispatcher: Option<Arc<dyn HookDispatcher>>) -> Self {
        self.assert_configuration_mutable("with_hook_dispatcher");
        self.hook_dispatcher = dispatcher;
        self
    }

    /// Inject the OAuth 2.1 + PKCE seam ([`OAuthDeps`]) for remote MCP servers.
    /// Builder-style so it composes with [`Self::new`] / [`Self::with_raw_conn`]:
    ///
    /// ```ignore
    /// let reg = McpRegistry::with_raw_conn(transport, raw_conn).with_oauth(deps);
    /// ```
    ///
    /// With this wired, [`Self::connect`] resolves a Bearer token for any
    /// `Sse{oauth:Some}` / `Http{oauth:Some}` server (load → refresh-on-expiry →
    /// interactive flow), injects `Authorization: Bearer <token>` into the
    /// spec's headers, and retries once after a 401. Static-token servers
    /// (`oauth: None`) take the unchanged path.
    #[must_use]
    pub fn with_oauth(mut self, deps: OAuthDeps) -> Self {
        self.assert_configuration_mutable("with_oauth");
        self.oauth = Some(deps);
        self
    }

    /// Use the session cwd for every dynamic headers helper.
    #[must_use]
    pub fn with_headers_helper_cwd(mut self, cwd: std::path::PathBuf) -> Self {
        self.assert_configuration_mutable("with_headers_helper_cwd");
        self.headers_helper_cwd = cwd;
        self
    }

    /// Associate a plugin MCP server with the plugin root used by its helper.
    pub async fn set_headers_helper_plugin_root(
        &self,
        server: impl Into<String>,
        root: std::path::PathBuf,
    ) {
        self.headers_helper_plugin_roots
            .write()
            .await
            .insert(server.into(), root);
    }

    /// Remove a plugin helper context when its owning plugin unloads.
    pub async fn remove_headers_helper_plugin_root(&self, server: &str) {
        self.headers_helper_plugin_roots
            .write()
            .await
            .remove(server);
    }

    /// Inject the session's LIVE additional working directories (settings
    /// `additionalDirectories` union CLI `--add-dir`) advertised alongside cwd
    /// on each connected server's `roots/list`. Builder-style so it composes
    /// with [`Self::new`] / [`Self::with_raw_conn`]:
    ///
    /// ```ignore
    /// let roots = mcp::new_shared_roots(vec![PathBuf::from("/tmp/extra")]);
    /// let reg = McpRegistry::with_raw_conn(transport, raw_conn)
    ///     .with_additional_roots(roots);
    /// ```
    ///
    /// Takes the SHARED [`crate::SharedRoots`] cell (not a snapshot) so the
    /// composition root can retain the SAME `Arc` and later push into it via
    /// [`Self::add_root`] to drive a runtime `/add-dir`. An empty cell (the
    /// default) leaves `roots/list` cwd-only (unchanged). Matches claude-code
    /// 2.1.207 `r1d()` = `[cwd, ...additionalWorkingDirectories]`.
    #[must_use]
    pub fn with_additional_roots(mut self, roots: crate::SharedRoots) -> Self {
        self.assert_configuration_mutable("with_additional_roots");
        self.additional_roots = roots;
        self
    }

    /// Push `dir` into the LIVE additional-roots set (the shared `roots/list`
    /// source) with a jzn-style change-compare: returns `true` when the dir was
    /// newly added, `false` when it was already present (a strict no-op). The
    /// caller only fires [`Self::notify_roots_list_changed_all`] on a `true`
    /// result — matching claude-code, which recomputes the sorted additional-dir
    /// list (`jzn`) and notifies MCP roots ONLY on a real change. Parity 2.1.207
    /// P1-08 runtime `/add-dir`.
    pub fn add_root(&self, dir: std::path::PathBuf) -> bool {
        let mut guard = self
            .additional_roots
            .write()
            .expect("additional_roots lock poisoned");
        if guard.iter().any(|d| d == &dir) {
            return false;
        }
        guard.push(dir);
        true
    }

    /// Snapshot of the LIVE additional-roots set (test / observability).
    #[must_use]
    pub fn additional_roots_snapshot(&self) -> Vec<std::path::PathBuf> {
        self.additional_roots
            .read()
            .expect("additional_roots lock poisoned")
            .clone()
    }

    /// Send `notifications/roots/list_changed` to EVERY connected MCP client,
    /// telling each server the client's working-dir set changed so it should
    /// re-query `roots/list`. A 1:1 port of claude-code's
    /// `notifyMcpRootsListChanged` → `UMy()` fan-out, which calls
    /// `sendRootsListChanged()` on every connected client. Best-effort per
    /// client (a per-client send failure is logged + swallowed inside
    /// [`McpClient::send_roots_list_changed`]). Returns the number of clients
    /// notified. Parity 2.1.207 P1-08.
    pub async fn notify_roots_list_changed_all(&self) -> usize {
        let clients: Vec<Arc<McpClient>> = self
            .clients
            .read()
            .await
            .values()
            .map(|entry| Arc::clone(&entry.client))
            .collect();
        for client in &clients {
            client.send_roots_list_changed();
        }
        clients.len()
    }

    /// Whether the OAuth seam ([`OAuthDeps`]) has been injected via
    /// [`Self::with_oauth`]. `false` leaves OAuth-configured remote servers on
    /// their static-header fallback; `true` enables the interactive
    /// load → refresh → consent flow. Used by the desktop composition-root test
    /// to assert OAuth is production-reachable.
    #[must_use]
    pub fn has_oauth(&self) -> bool {
        self.oauth.is_some()
    }

    /// Whether a discovery-cache store has been injected via
    /// [`Self::with_discovery_cache_store`]. Used by composition-root tests to
    /// prove the otherwise optional cache is production-reachable.
    #[must_use]
    pub fn has_discovery_cache_store(&self) -> bool {
        self.discovery_cache_store.is_some()
    }

    /// Whether any shared MCP server currently carries the coordinator-only
    /// `role:"comms"` marker.  Cached catalogs count as well as live
    /// connections, because routing decisions must not change during lazy
    /// dialing.
    pub async fn has_comms_roled_server(&self) -> bool {
        self.connections.read().await.values().any(|state| {
            matches!(
                state,
                McpConnectionState::Connected { config, .. }
                    | McpConnectionState::Cached { config, .. }
                    if config.metadata.role == Some(crate::connection::McpServerRole::Comms)
            )
        })
    }

    /// Whether a Cross-App-Access ([`XaaConfigProvider`]) provider is wired into
    /// the injected [`OAuthDeps`]. `false` (the default, and the state when no
    /// `xaaIdp` settings tier is present) leaves an `oauth.xaa` server on its
    /// actionable hard-fail; `true` means the host can supply the IdP `id_token`
    /// + AS credentials and the XAA token-exchange chain can run. Used by the
    /// desktop composition-root test to assert the XAA seam is reachable when
    /// configured.
    #[must_use]
    pub fn has_xaa(&self) -> bool {
        self.oauth.as_ref().is_some_and(|d| d.xaa_config.is_some())
    }

    /// Cache an `Arc<McpClient>` for `name` (M4-07).
    ///
    /// The platform host calls this after building the client (typically
    /// alongside `connect`). Builtin tools then call [`Self::get_client`]
    /// to dispatch over the wire-locked client surface.
    pub async fn register_client(&self, name: &str, client: Arc<McpClient>) {
        self.clients.write().await.insert(
            name.into(),
            RegisteredClient {
                connection_id: None,
                client,
            },
        );
    }

    /// Return the cached `Arc<McpClient>` for `name`, if any (M4-07).
    ///
    /// Matches by NORMALIZED key: a model-supplied `<server>` token is the
    /// normalized form (`mcp__<normalize(server)>__<tool>`), while clients are
    /// stored under the RAW `config.name` (so `/mcp` shows the raw display
    /// name). This mirrors claude-code's `normalizeNameForMCP(client.name) ===
    /// serverName` lookup (normalization.rs:22-24, client.ts).
    pub async fn get_client(&self, name: &str) -> Option<Arc<McpClient>> {
        let clients = self.clients.read().await;
        if let Some(entry) = clients.get(name) {
            return Some(Arc::clone(&entry.client));
        }
        let normalized = normalize_name_for_mcp(name);
        clients
            .iter()
            .find(|(k, _)| normalize_name_for_mcp(k) == normalized)
            .map(|(_, entry)| Arc::clone(&entry.client))
    }

    /// Return a live [`McpClient`] for `name`, lazily dialing a discovery-cache
    /// hit when needed. `name` is matched by normalized form, exactly like
    /// [`Self::get_client`].
    pub async fn ensure_connected_client(&self, name: &str) -> Result<Arc<McpClient>, McpError> {
        if let Some(client) = self.get_client(name).await {
            return Ok(client);
        }
        #[cfg(test)]
        self.maybe_pause_after_initial_client_miss().await;
        if let Some(raw_key) = self.cached_raw_key(name).await {
            self.ensure_dialed_from_cache(&raw_key).await?;
            return self.get_client(name).await.ok_or_else(|| {
                McpError::Internal(format!(
                    "MCP server \"{name}\" did not publish a client after cache lazy-dial"
                ))
            });
        }
        if let Some(client) = self.get_client(name).await {
            return Ok(client);
        }
        Err(McpError::Internal(format!(
            "MCP server \"{name}\" has no live client"
        )))
    }

    /// Whether `name` can currently accept tool calls.
    ///
    /// Most transports dispatch through a live [`McpClient`]. Same-process
    /// providers are deliberately different: their [`McpTransport`] already
    /// is the invocation boundary, so requiring an otherwise-unused JSON-RPC
    /// connection would turn a successfully discovered `InProcess` server
    /// into an uncallable catalog. Only a connected `InProcess` server may use
    /// this direct path; stdio and network transports remain fail-closed on a
    /// missing client.
    pub async fn has_callable_server(&self, name: &str) -> bool {
        self.get_client(name).await.is_some()
            || self.direct_inprocess_connection(name).await.is_some()
            // §11 Stage 2: a `Cached` server has no registered client (`clients`
            // is a separate map from `connections`, and a cache hit never
            // populates it — see `McpConnectionState::Cached`'s doc), but it
            // MUST still report callable: `call_tool_with_auth_retry`'s lazy
            // dial upgrades it to a real `Connected` client on first use.
            // Without this arm a cached server would be dropped from dispatch
            // entirely, defeating the whole point of caching it.
            || self.cached_raw_key(name).await.is_some()
    }

    /// The RAW stored key of a cache-served entry matching `name` by
    /// NORMALIZED form (same lookup convention as [`Self::get_client`]).
    ///
    /// This includes both a visible [`McpConnectionState::Cached`] entry and
    /// its detached lazy-upgrade successor [`McpConnectionState::Connecting`]
    /// while the per-key slot is still active, so late waiters can join the
    /// owner instead of falling through to a spurious "no live client".
    async fn cached_raw_key(&self, name: &str) -> Option<String> {
        let candidate = {
            let connections = self.connections.read().await;
            if let Some(state) = connections.get(name) {
                match state {
                    McpConnectionState::Cached { .. } => return Some(name.to_string()),
                    McpConnectionState::Connecting { .. } => Some((name.to_string(), true)),
                    _ => None,
                }
            } else {
                let normalized = normalize_name_for_mcp(name);
                connections.iter().find_map(|(raw_name, state)| {
                    if normalize_name_for_mcp(raw_name) != normalized {
                        return None;
                    }
                    match state {
                        McpConnectionState::Cached { .. } => Some((raw_name.clone(), false)),
                        McpConnectionState::Connecting { .. } => Some((raw_name.clone(), true)),
                        _ => None,
                    }
                })
            }
        };
        match candidate {
            Some((key, false)) => Some(key),
            Some((key, true)) => self.lazy_upgrade_slot(&key).await.map(|_| key),
            None => None,
        }
    }

    /// §11 Stage 2 — lazy dial: upgrade a `Cached` entry stored under the RAW
    /// key `key` to a real `Connected` one by running the ordinary connect
    /// path (a lazily-dialed cached server IS a fresh connection — the
    /// transport was simply never opened yet). Single-flighted through the
    /// SAME per-server [`Self::lifecycle_lock`] every other connect path
    /// uses, so two concurrent tool calls against the same cached server
    /// dial exactly once: the second caller blocks on the lock, then
    /// re-reads the state and finds `Connected` already, returning its id
    /// with no second dial.
    ///
    /// Returns the live [`McpConnectionId`] on success. Returns an error
    /// (never silently no-ops) when `key` is no longer `Cached` by the time
    /// the lock is acquired AND is not already `Connected` either (e.g. it
    /// was disconnected/removed concurrently) — the caller (dispatch) then
    /// falls through to its existing "no live client" failure.
    async fn ensure_dialed_from_cache(&self, key: &str) -> Result<McpConnectionId, McpError> {
        let lifecycle = self.lifecycle_lock(key);
        let outcome = {
            let _guard = lifecycle.lock().await;
            let negotiation_mode = if let Some(slot) = self.lazy_upgrade_slot(key).await {
                slot.negotiation_mode
            } else {
                let conns = self.connections.read().await;
                match conns.get(key) {
                    Some(McpConnectionState::Cached { config, .. })
                    | Some(McpConnectionState::Connecting { config, .. }) => {
                        crate::protocol_negotiation::resolve_for_spec_with_transport(
                            &config.spec,
                            config.metadata.transport.as_deref(),
                            mcp_connection_timeout().as_millis() as u64,
                        )
                    }
                    _ => crate::protocol_negotiation::NegotiationMode::Legacy,
                }
            };
            self.prepare_lazy_upgrade_slot_locked(
                key,
                LazyUpgradeMode::Foreground,
                None,
                None,
                negotiation_mode,
            )
            .await?
        };
        match outcome {
            LazyUpgradePreparation::Connected(connection_id) => Ok(connection_id),
            LazyUpgradePreparation::Wait(slot, should_spawn) => {
                if should_spawn {
                    self.spawn_lazy_upgrade_owner(key.to_string(), slot.clone());
                }
                slot.wait().await
            }
            LazyUpgradePreparation::Skip => Err(McpError::Connection(format!(
                "MCP server \"{key}\" is no longer cached"
            ))),
        }
    }

    async fn direct_inprocess_connection(&self, name: &str) -> Option<(McpConnectionId, String)> {
        let connections = self.connections.read().await;
        connections.iter().find_map(|(raw_name, state)| {
            if normalize_name_for_mcp(raw_name) != name {
                return None;
            }
            match state {
                McpConnectionState::Connected {
                    config:
                        McpServerConfig {
                            spec: McpTransportSpec::InProcess { registry_key },
                            ..
                        },
                    connection_id,
                    ..
                } => Some((*connection_id, registry_key.clone())),
                _ => None,
            }
        })
    }

    /// Return the [`McpServerConfig`] for `name`, if any (M4-07).
    ///
    /// Reads the current state-map; returns the config from any variant
    /// that carries one. Matches by NORMALIZED key (see [`Self::get_client`])
    /// so a model-supplied `<server>` token resolves a raw stored key.
    pub async fn get_config(&self, name: &str) -> Option<McpServerConfig> {
        let conns = self.connections.read().await;
        conns
            .iter()
            .find(|(k, _)| normalize_name_for_mcp(k) == name)
            .map(|(_, v)| v.config().clone())
    }

    /// Call one MCP tool and retry a single authentication failure after a
    /// full reconnect. Reconnect re-runs `headersHelper` and OAuth resolution;
    /// a second 401/403 is returned unchanged and never loops.
    pub async fn call_tool_with_auth_retry(
        &self,
        server: &str,
        full_name: &str,
        input: serde_json::Value,
        tool_use_id: Option<&str>,
        on_progress: Option<crate::client::McpProgressCallback>,
    ) -> Result<lingxi_core::host::McpToolResultDto, crate::client::McpClientError> {
        let mut lazy_dialed = false;
        let client = if let Some(client) = self.get_client(server).await {
            Some(client)
        } else {
            // ⚠️ Do NOT collapse this into `else if`. `clippy --fix` did exactly
            // that on 2026-09-16 and silently dropped both the `cfg(test)` pause
            // below and the comment explaining the stage, which deadlocked 12
            // registry tests that wait on it.
            #[cfg(test)]
            self.maybe_pause_after_initial_client_miss().await;
            // §11 Stage 2 — lazy dial: a `Cached` server was served from disk
            // at connect time with no transport ever opened, so it has no
            // registered client yet. The FIRST tool call against it dials for
            // real (single-flighted via `ensure_dialed_from_cache`'s
            // lifecycle lock), then dispatches through the now-real client.
            if let Some(raw_key) = self.cached_raw_key(server).await {
                lazy_dialed = true;
                self.ensure_dialed_from_cache(&raw_key)
                    .await
                    .map_err(|error| crate::client::McpClientError::Rpc(error.to_string()))?;
                self.get_client(server).await
            } else {
                self.get_client(server).await
            }
        };
        let Some(client) = client else {
            if lazy_dialed {
                return Err(crate::client::McpClientError::Rpc(format!(
                    "MCP server \"{server}\" did not publish a client after cache lazy-dial"
                )));
            }
            let Some((connection_id, _registry_key)) =
                self.direct_inprocess_connection(server).await
            else {
                return Err(crate::client::McpClientError::Rpc(format!(
                    "MCP server \"{server}\" has no live client"
                )));
            };

            // `full_name` is the model-facing `mcp__<server>__<tool>` name.
            // The caller has already resolved the final segment back to the
            // server's raw tool name, so stripping the fixed prefix is safe and
            // avoids teaching an in-process provider about FQN normalization.
            let prefix = format!("mcp__{server}__");
            let tool_name = full_name.strip_prefix(&prefix).ok_or_else(|| {
                crate::client::McpClientError::Rpc(format!(
                    "invalid MCP tool name {full_name:?} for server {server:?}"
                ))
            })?;
            return self
                .transport
                .call_tool(&McpRawConnection { connection_id }, tool_name, input)
                .await
                .map_err(|error| crate::client::McpClientError::Rpc(error.to_string()));
        };
        let first = client
            .call_tool_with_progress(full_name, input.clone(), tool_use_id, on_progress.clone())
            .await;
        let Err(error) = first else {
            return first;
        };
        let raw_name = {
            let connections = self.connections.read().await;
            connections
                .keys()
                .find(|name| normalize_name_for_mcp(name) == server)
                .cloned()
        };
        let config = if let Some(raw_name) = raw_name.as_deref() {
            self.get_config(raw_name).await
        } else {
            None
        };
        let session_expired = config
            .as_ref()
            .is_some_and(|config| config.spec.kind() == "http" && error.is_session_expired());
        if !error.is_auth_response() && !session_expired {
            return Err(error);
        }

        if session_expired {
            let config = config
                .as_ref()
                .expect("session_expired implies config was resolved");
            let identity = oauth::McpOAuthTelemetryContext::for_server(&config.name, &config.spec);
            emit_session_expired(&telemetry::tengu::mcp::SessionExpiredPayload {
                error_code: error
                    .session_expired_error_code()
                    .map(|code| telemetry::Verified::assert_safe(code.to_string())),
                transport_type: telemetry::Verified::assert_safe(
                    config
                        .metadata
                        .transport
                        .clone()
                        .unwrap_or_else(|| config.spec.kind().to_string()),
                ),
                mcp_server_key_hash: identity.mcp_server_key_hash,
                mcp_server_base_url: telemetry_mcp_server_base_url(&config.spec),
            });
        }

        let raw_name =
            raw_name.ok_or_else(|| crate::client::McpClientError::Rpc(error.to_string()))?;
        if session_expired {
            self.reconnect_preserving_auth(&raw_name).await
        } else {
            self.reconnect(&raw_name).await
        }
        .map_err(|retry_error| crate::client::McpClientError::Rpc(retry_error.to_string()))?;
        let mut _refreshed_from_cache = false;
        let refreshed = if let Some(client) = self.get_client(server).await {
            Some(client)
        } else if let Some(raw_key) = self.cached_raw_key(server).await {
            _refreshed_from_cache = true;
            self.ensure_dialed_from_cache(&raw_key)
                .await
                .map_err(|retry_error| {
                    crate::client::McpClientError::Rpc(retry_error.to_string())
                })?;
            self.get_client(server).await
        } else {
            self.get_client(server).await
        }
        .ok_or_else(|| {
            crate::client::McpClientError::Rpc(format!(
                "MCP server \"{server}\" did not publish a client after authentication refresh"
            ))
        })?;
        let second = refreshed
            .call_tool_with_progress(full_name, input, tool_use_id, on_progress)
            .await;
        if let Err(error) = &second {
            if error.is_auth_response() {
                if let Some(config) = self.get_config(&raw_name).await {
                    emit_tool_call_auth_error_for_config(
                        &config,
                        tool_call_auth_error_code(error),
                        telemetry::tengu::mcp::ToolCallAuthErrorKind::TokenExpired,
                    );
                }
            }
        }
        second
    }

    /// Test-only helper that registers a config (`Disconnected` state) and
    /// caches a pre-built `Arc<McpClient>` for `name`. Used by M4-07 tools
    /// unit tests to exercise the dispatch paths without spinning up a
    /// real transport.
    #[doc(hidden)]
    pub async fn register_test_client(
        &self,
        name: &str,
        config: McpServerConfig,
        client: Arc<McpClient>,
    ) {
        self.connections.write().await.insert(
            name.into(),
            McpConnectionState::Disconnected {
                config,
                last_error: None,
            },
        );
        self.clients.write().await.insert(
            name.into(),
            RegisteredClient {
                connection_id: None,
                client,
            },
        );
    }

    /// Connect, run `initialize`, and discover the server's catalog.
    ///
    /// Re-uses the existing connection if `config.name` is already in the
    /// `Connected` state.
    pub async fn connect(&self, config: McpServerConfig) -> Result<McpConnectionId, McpError> {
        self.freeze_configuration();
        // `zar()` — refuse an unconfigured remote server BEFORE opening a
        // socket or spawning anything. Oracle @231408727:
        //   if (zar(t)) return {type:"failed", errorCode:"UNCONFIGURED", ...}
        //
        // This guard is what makes it safe for `json_config` to keep
        // blank-url entries: they now reach the listing, and `mcp list`
        // health-probes approved servers, so without it a typo'd config would
        // become a live connect attempt against an empty URL.
        if is_unconfigured_remote(&config.spec) {
            emit_server_connection_failed(&server_connection_failed_payload(
                &config,
                None,
                None,
                Some("UNCONFIGURED"),
            ));
            return Err(McpError::Connection(UNCONFIGURED_MESSAGE.to_string()));
        }
        let lifecycle = self.lifecycle_lock(&config.name);
        let _guard = lifecycle.lock().await;
        self.connect_locked(config, None).await
    }

    /// Build a replacement connection completely before publishing it, then
    /// swap the named registry slot in one state/client write. A failed
    /// candidate leaves the previous connected generation callable.
    ///
    /// Hosts use this for live settings reconciliation. It intentionally does
    /// not discover an absent configuration or change source precedence; the
    /// caller supplies the already parsed, policy-gated winning config.
    pub async fn replace_config_atomically(
        &self,
        config: McpServerConfig,
    ) -> Result<Option<McpConnectionId>, McpError> {
        self.freeze_configuration();
        let key = config.name.clone();
        let lifecycle = self.lifecycle_lock(&key);
        let _guard = lifecycle.lock().await;
        Self::validate_connectable_config(&config)?;

        let (current_config, retired_connection_id, retired_is_live) = {
            let connections = self.connections.read().await;
            match connections.get(&key) {
                Some(McpConnectionState::Connected {
                    config,
                    connection_id,
                    ..
                })
                | Some(McpConnectionState::HealthChecking {
                    config,
                    connection_id,
                }) => (Some(config.clone()), Some(*connection_id), true),
                Some(McpConnectionState::Cached {
                    config,
                    connection_id,
                    ..
                }) => (Some(config.clone()), Some(*connection_id), false),
                Some(state) => (Some(state.config().clone()), None, false),
                None => (None, None, false),
            }
        };
        if current_config
            .as_ref()
            .is_some_and(|current| Self::same_config_snapshot(current, &config))
        {
            return Ok(retired_connection_id);
        }

        if config.disabled {
            if let Some(connection_id) = retired_connection_id.filter(|_| retired_is_live) {
                self.transport.disconnect(connection_id).await?;
            }
            self.connections.write().await.insert(
                key.clone(),
                McpConnectionState::Disconnected {
                    config: config.clone(),
                    last_error: None,
                },
            );
            self.clients.write().await.shift_remove(&key);
            self.clear_prompt_predecessors_for_key(&key).await;
            if let Some(connection_id) = retired_connection_id {
                self.emit_retire_event_if_shared(&config, &key, connection_id)
                    .await;
            }
            return Ok(None);
        }

        let negotiation_mode = crate::protocol_negotiation::resolve_for_spec_with_transport(
            &config.spec,
            config.metadata.transport.as_deref(),
            mcp_connection_timeout().as_millis() as u64,
        );
        let discovery = self
            .discover_live_connection(&config, negotiation_mode)
            .await?;
        let connection_id = self
            .install_live_discovery(key, config, discovery, retired_connection_id, None, None)
            .await?;
        if let Some(retired) = retired_connection_id.filter(|_| retired_is_live) {
            self.disconnect_or_schedule_cleanup(retired).await;
        }
        Ok(Some(connection_id))
    }

    /// Connect a server only while a host-owned reconciliation generation is
    /// current. The guard is checked after the server lifecycle lock is
    /// acquired and again immediately before cache/live state publication.
    /// `None` means that the operation was superseded, or that another config
    /// is already installed for this name; in either case the registry is
    /// left untouched.
    pub async fn connect_if_current(
        &self,
        config: McpServerConfig,
        guard: Arc<McpOperationGuard>,
    ) -> Result<Option<McpConnectionId>, McpError> {
        self.freeze_configuration();
        if is_unconfigured_remote(&config.spec) {
            emit_server_connection_failed(&server_connection_failed_payload(
                &config,
                None,
                None,
                Some("UNCONFIGURED"),
            ));
            return Err(McpError::Connection(UNCONFIGURED_MESSAGE.to_string()));
        }
        let key = config.name.clone();
        let lifecycle = self.lifecycle_lock(&key);
        let _guard = lifecycle.lock().await;
        if !guard() {
            return Ok(None);
        }
        if config.disabled {
            let mut connections = self.connections.write().await;
            if !guard() {
                return Ok(None);
            }
            match connections.get(&key) {
                Some(McpConnectionState::Connected { .. })
                | Some(McpConnectionState::Cached { .. })
                | Some(McpConnectionState::HealthChecking { .. })
                | Some(McpConnectionState::Connecting { .. })
                | Some(McpConnectionState::AwaitingOAuth { .. })
                | Some(McpConnectionState::Reconnecting { .. }) => {}
                _ => {
                    connections.insert(
                        key.clone(),
                        McpConnectionState::Disconnected {
                            config,
                            last_error: None,
                        },
                    );
                }
            }
            return Ok(None);
        }
        {
            let conns = self.connections.read().await;
            match conns.get(&key) {
                Some(McpConnectionState::Connected {
                    connection_id,
                    config: current,
                    ..
                })
                | Some(McpConnectionState::Cached {
                    connection_id,
                    config: current,
                    ..
                }) => {
                    return Ok(
                        Self::same_config_snapshot(current, &config).then_some(*connection_id)
                    );
                }
                _ => {}
            }
        }

        self.kick_pending_transport_cleanups().await;
        let result = self
            .connect_locked_inner_with_guard(config.clone(), None, Some(&*guard))
            .await;
        match result {
            Ok(connection_id) => Ok(Some(connection_id)),
            Err(error) if is_operation_guard_rejected(&error) => {
                // A live discovery can become stale after the Connecting
                // marker is published but before its result is installed.
                // Retire that marker through the normal lifecycle cleanup so
                // clients, lazy slots, catalog partitions, and cache family
                // state cannot be stranded behind an expired generation.
                self.cleanup_owned_pending_state(&key, &config).await?;
                Ok(None)
            }
            Err(error) => {
                if !guard() {
                    // The operation failed after a newer reload invalidated
                    // this generation. Never publish its stale failure state;
                    // only retire a Connecting marker that still carries this
                    // operation's exact config through normal registry cleanup.
                    let _ = self.cleanup_owned_pending_state(&key, &config).await;
                    return Ok(None);
                }
                let mut connections = self.connections.write().await;
                if !guard() {
                    drop(connections);
                    let _ = self.cleanup_owned_pending_state(&key, &config).await;
                    return Ok(None);
                }
                connections.insert(
                    key.clone(),
                    McpConnectionState::Disconnected {
                        config,
                        last_error: Some(error.to_string()),
                    },
                );
                drop(connections);
                self.clear_prompt_predecessors_for_key(&key).await;
                Err(error)
            }
        }
    }

    async fn cleanup_owned_pending_state(
        &self,
        key: &str,
        expected_config: &McpServerConfig,
    ) -> Result<(), McpError> {
        let own_connecting_state = self.connections.read().await.get(key).is_some_and(|state| {
            matches!(
                state,
                McpConnectionState::Connecting { config: current, .. }
                    | McpConnectionState::AwaitingOAuth { config: current, .. }
                    if Self::same_config_snapshot(current, expected_config)
            )
        });
        if own_connecting_state {
            self.disconnect_locked_inner(key, false, true).await?;
        }
        Ok(())
    }

    /// §24b: connect a per-SUBAGENT inline `mcpServers` entry (claude `Agr`'s
    /// `connectToServer(name, config, ...)`, invoked once per subagent
    /// spawn). Registers the connection under a table key namespaced by
    /// `agent_id` ([`agent_scope_table_key`]) so two concurrent subagents
    /// that each declare an inline server sharing the same plain
    /// `config.name` never clobber each other's connection state or dispatch
    /// target — `config.name` itself, `McpTransportSpec::kind()`, and header
    /// construction are all UNCHANGED, so the model-facing FQN, permission
    /// rule matching, and `oauth::server_key` (which hashes `config.name`)
    /// stay exactly as they are for a shared/session-level connect.
    ///
    /// Returns the connection id plus the table key the caller must retain to
    /// build the per-tool [`MCPTool::bound_server_key`]-equivalent dispatch
    /// target (`tool_mcp`'s per-agent tool builder) and to later call
    /// [`Self::disconnect_agent_scoped`].
    ///
    /// Does NOT fire [`Self::catalog_changes`] on success — that broadcast
    /// drives the SHARED session `ToolRegistry`'s auto-register-on-connect
    /// listener (`apps/engine-desktop`/`apps/engine-mobile`), and an
    /// agent-scoped connection must never become visible outside the
    /// subagent that opened it.
    pub async fn connect_agent_scoped(
        &self,
        config: McpServerConfig,
        agent_id: AgentId,
    ) -> Result<(McpConnectionId, String), McpError> {
        self.freeze_configuration();
        if is_unconfigured_remote(&config.spec) {
            emit_server_connection_failed(&server_connection_failed_payload(
                &config,
                None,
                None,
                Some("UNCONFIGURED"),
            ));
            return Err(McpError::Connection(UNCONFIGURED_MESSAGE.to_string()));
        }
        let table_key = agent_scope_table_key(agent_id, &config.name);
        let lifecycle = self.lifecycle_lock(&table_key);
        let _guard = lifecycle.lock().await;
        let id = self.connect_locked(config, Some(table_key.clone())).await?;
        Ok((id, table_key))
    }

    /// Tear down an agent-scoped connection opened by
    /// [`Self::connect_agent_scoped`] — the port's equivalent of the oracle's
    /// per-client `cleanup()` in `Agr`'s returned closure. A PLAIN transport
    /// close: unlike [`Self::disconnect`]/[`Self::remove`] (explicit
    /// user-facing `/mcp` actions), this does NOT revoke any stored OAuth/XAA
    /// token — revoking a real persisted grant merely because one subagent
    /// spawn finished using it would silently log the user out of that
    /// server for every future session. No-op when nothing is connected
    /// under `table_key` (e.g. the connect attempt itself failed, so the
    /// oracle's `isNewlyCreated` client was never actually live).
    pub async fn disconnect_agent_scoped(&self, table_key: &str) -> Result<(), McpError> {
        let lifecycle = self.lifecycle_lock(table_key);
        let _guard = lifecycle.lock().await;
        let invalidated_slot = self.invalidate_lazy_upgrade_slot(table_key).await;
        let connection_id = {
            let conns = self.connections.read().await;
            match conns.get(table_key) {
                Some(McpConnectionState::Connected { connection_id, .. }) => Some(*connection_id),
                _ => None,
            }
        };
        if let Some(connection_id) = connection_id {
            self.transport.disconnect(connection_id).await?;
        }
        self.connections.write().await.remove(table_key);
        self.clients.write().await.shift_remove(table_key);
        self.clear_prompt_predecessors_for_key(table_key).await;
        Self::finish_invalidated_lazy_upgrade_slot(invalidated_slot.as_ref());
        Ok(())
    }

    async fn connect_locked(
        &self,
        config: McpServerConfig,
        table_key: Option<String>,
    ) -> Result<McpConnectionId, McpError> {
        self.freeze_configuration();
        self.kick_pending_transport_cleanups().await;
        let key = table_key.clone().unwrap_or_else(|| config.name.clone());
        let result = self.connect_locked_inner(config.clone(), table_key).await;
        if let Err(error) = &result {
            // A failed public connect must never strand the registry in
            // `Connecting`. Reconnect scheduling only considers disconnected
            // states, and `/mcp` should expose the actual last failure.
            self.connections.write().await.insert(
                key.clone(),
                McpConnectionState::Disconnected {
                    config,
                    last_error: Some(error.to_string()),
                },
            );
            self.clear_prompt_predecessors_for_key(&key).await;
        }
        result
    }

    async fn connect_locked_inner(
        &self,
        config: McpServerConfig,
        table_key: Option<String>,
    ) -> Result<McpConnectionId, McpError> {
        self.connect_locked_inner_with_guard(config, table_key, None)
            .await
    }

    async fn connect_locked_inner_with_guard(
        &self,
        config: McpServerConfig,
        table_key: Option<String>,
        operation_guard: Option<&McpOperationGuard>,
    ) -> Result<McpConnectionId, McpError> {
        let key = table_key.clone().unwrap_or_else(|| config.name.clone());
        Self::validate_connectable_config(&config)?;
        if operation_guard.is_some_and(|guard| !guard()) {
            return Err(operation_guard_rejected());
        }
        {
            let conns = self.connections.read().await;
            match conns.get(&key) {
                Some(McpConnectionState::Connected { connection_id, .. })
                | Some(McpConnectionState::Cached { connection_id, .. }) => {
                    return Ok(*connection_id);
                }
                _ => {}
            }
        }
        let invalidated_slot = self.invalidate_lazy_upgrade_slot(&key).await;
        Self::finish_invalidated_lazy_upgrade_slot(invalidated_slot.as_ref());

        let connect_timeout = mcp_connection_timeout();
        let negotiation_mode = crate::protocol_negotiation::resolve_for_spec_with_transport(
            &config.spec,
            config.metadata.transport.as_deref(),
            connect_timeout.as_millis() as u64,
        );
        if let Some(consult) = self
            .discovery_cache_decision_for(&config, negotiation_mode)
            .await
        {
            let DiscoveryCacheConsult {
                decision,
                partition,
            } = consult;
            match decision {
                crate::discovery_cache::Decision::Fresh { entry, age_ms } => {
                    return self
                        .serve_discovery_cache_hit(
                            &config,
                            &key,
                            entry,
                            age_ms,
                            true,
                            operation_guard,
                        )
                        .await;
                }
                crate::discovery_cache::Decision::Stale { entry, age_ms } => {
                    let entry_era = entry
                        .negotiated_era
                        .clone()
                        .unwrap_or_else(|| "legacy".into());
                    let connection_id = self
                        .serve_discovery_cache_hit(
                            &config,
                            &key,
                            entry,
                            age_ms,
                            false,
                            operation_guard,
                        )
                        .await?;
                    if let LazyUpgradePreparation::Wait(slot, true) = self
                        .prepare_lazy_upgrade_slot_locked(
                            &key,
                            LazyUpgradeMode::Background,
                            partition,
                            Some(entry_era),
                            negotiation_mode,
                        )
                        .await?
                    {
                        self.spawn_lazy_upgrade_owner(key.clone(), slot);
                    }
                    return Ok(connection_id);
                }
                crate::discovery_cache::Decision::Miss { reason } => {
                    if let Some(source) =
                        discovery_source_emission(&crate::discovery_cache::Decision::Miss {
                            reason,
                        })
                    {
                        telemetry::emit_mcp_discovery_source(
                            &telemetry::tengu::mcp::DiscoverySourcePayload {
                                transport_type: telemetry::pii::Verified::assert_safe(
                                    config.spec.kind().to_string(),
                                ),
                                source: telemetry::pii::Verified::assert_safe(source.to_string()),
                                entry_age_ms: None,
                            },
                        );
                    }
                }
            }
        }

        let mut connections = self.connections.write().await;
        if operation_guard.is_some_and(|guard| !guard()) {
            return Err(operation_guard_rejected());
        }
        connections.insert(
            key.clone(),
            McpConnectionState::Connecting {
                config: config.clone(),
                started_at: SystemTime::now(),
            },
        );
        drop(connections);
        let discovery = match self
            .discover_live_connection(&config, negotiation_mode)
            .await
        {
            Ok(discovery) => discovery,
            Err(error) => {
                if error_is_auth_response(&error) {
                    emit_server_needs_auth_for_config(&config, None);
                } else if matches!(error, McpError::OAuth(_)) {
                    emit_server_needs_auth_for_config(&config, Some("discovery_schema"));
                }
                return Err(error);
            }
        };
        self.install_live_discovery(key, config, discovery, None, operation_guard, None)
            .await
    }

    fn validate_connectable_config(config: &McpServerConfig) -> Result<(), McpError> {
        if config.is_unconfigured() {
            emit_server_connection_failed(&server_connection_failed_payload(
                config,
                None,
                None,
                Some("UNCONFIGURED"),
            ));
            return Err(McpError::Connection(
                crate::connection::UNCONFIGURED_ERROR.to_string(),
            ));
        }
        if let Some(err) = &config.config_error {
            emit_server_config_invalid(config, telemetry::tengu::mcp::ConfigInvalidSource::Loader);
            emit_server_connection_failed(&server_connection_failed_payload(
                config,
                None,
                None,
                Some("INVALID_CONFIG"),
            ));
            return Err(McpError::Connection(err.clone()));
        }
        if let Some(err) = config.connect_time_url_error() {
            emit_server_config_invalid(config, telemetry::tengu::mcp::ConfigInvalidSource::Connect);
            emit_server_connection_failed(&server_connection_failed_payload(
                config,
                None,
                None,
                Some("INVALID_CONFIG"),
            ));
            return Err(McpError::Connection(err.to_string()));
        }
        Ok(())
    }

    fn stable_config_signature(config: &McpServerConfig) -> Option<serde_json::Value> {
        serde_json::to_value(config).ok()
    }

    fn same_config_snapshot(left: &McpServerConfig, right: &McpServerConfig) -> bool {
        Self::stable_config_signature(left) == Self::stable_config_signature(right)
    }

    async fn lazy_upgrade_slot(&self, key: &str) -> Option<Arc<LazyUpgradeSlot>> {
        self.lazy_upgrade_slots.read().await.get(key).cloned()
    }

    async fn invalidate_lazy_upgrade_slot(&self, key: &str) -> Option<Arc<LazyUpgradeSlot>> {
        self.lazy_upgrade_slots.write().await.remove(key)
    }

    async fn remove_lazy_upgrade_slot_if_matches(&self, key: &str, slot: &Arc<LazyUpgradeSlot>) {
        let mut slots = self.lazy_upgrade_slots.write().await;
        if matches!(slots.get(key), Some(current) if Arc::ptr_eq(current, slot)) {
            slots.remove(key);
        }
    }

    fn finish_invalidated_lazy_upgrade_slot(slot: Option<&Arc<LazyUpgradeSlot>>) {
        if let Some(slot) = slot {
            slot.finish_if_unset(LazyUpgradeTerminal::Error(Arc::new(slot.stale_error())));
        }
    }

    async fn clear_prompt_predecessors_for_key(&self, key: &str) {
        self.prompt_predecessors
            .write()
            .await
            .retain(|_, predecessor| predecessor.key != key);
    }

    async fn prepare_lazy_upgrade_slot_locked(
        &self,
        key: &str,
        mode: LazyUpgradeMode,
        refresh_partition: Option<DiscoveryCachePartition>,
        refresh_entry_era: Option<String>,
        negotiation_mode: crate::protocol_negotiation::NegotiationMode,
    ) -> Result<LazyUpgradePreparation, McpError> {
        enum CachedDialState {
            Connected(McpConnectionId),
            Cached {
                connection_id: McpConnectionId,
                config: McpServerConfig,
            },
            Connecting {
                config: McpServerConfig,
            },
            Missing,
        }

        let state = {
            let conns = self.connections.read().await;
            match conns.get(key) {
                Some(McpConnectionState::Connected { connection_id, .. }) => {
                    CachedDialState::Connected(*connection_id)
                }
                Some(McpConnectionState::Cached {
                    connection_id,
                    config,
                    ..
                }) => CachedDialState::Cached {
                    connection_id: *connection_id,
                    config: config.clone(),
                },
                Some(McpConnectionState::Connecting { config, .. }) => {
                    CachedDialState::Connecting {
                        config: config.clone(),
                    }
                }
                _ => CachedDialState::Missing,
            }
        };

        match state {
            CachedDialState::Connected(connection_id) => {
                Ok(LazyUpgradePreparation::Connected(connection_id))
            }
            CachedDialState::Cached {
                connection_id,
                config,
            } => {
                if let Some(slot) = self.lazy_upgrade_slot(key).await {
                    if slot.matches(connection_id, &config) {
                        return Ok(LazyUpgradePreparation::Wait(slot, false));
                    }
                    if let Some(slot) = self.invalidate_lazy_upgrade_slot(key).await {
                        slot.finish_if_unset(LazyUpgradeTerminal::Error(Arc::new(
                            slot.stale_error(),
                        )));
                    }
                }
                let slot = Arc::new(LazyUpgradeSlot::new(
                    key.to_string(),
                    connection_id,
                    config.clone(),
                    refresh_partition,
                    refresh_entry_era,
                    negotiation_mode,
                    mode,
                ));
                if mode == LazyUpgradeMode::Foreground {
                    self.connections.write().await.insert(
                        key.to_string(),
                        McpConnectionState::Connecting {
                            config,
                            started_at: SystemTime::now(),
                        },
                    );
                }
                self.lazy_upgrade_slots
                    .write()
                    .await
                    .insert(key.to_string(), slot.clone());
                Ok(LazyUpgradePreparation::Wait(slot, true))
            }
            CachedDialState::Connecting { config } => {
                let Some(slot) = self.lazy_upgrade_slot(key).await else {
                    return match mode {
                        LazyUpgradeMode::Foreground => Err(McpError::Connection(format!(
                            "MCP server \"{key}\" is no longer cached"
                        ))),
                        LazyUpgradeMode::Background => Ok(LazyUpgradePreparation::Skip),
                    };
                };
                if Self::same_config_snapshot(&slot.expected_config, &config) {
                    Ok(LazyUpgradePreparation::Wait(slot, false))
                } else {
                    match mode {
                        LazyUpgradeMode::Foreground => Err(McpError::Connection(format!(
                            "MCP server \"{key}\" is no longer cached"
                        ))),
                        LazyUpgradeMode::Background => Ok(LazyUpgradePreparation::Skip),
                    }
                }
            }
            CachedDialState::Missing => match mode {
                LazyUpgradeMode::Foreground => Err(McpError::Connection(format!(
                    "MCP server \"{key}\" is no longer cached"
                ))),
                LazyUpgradeMode::Background => Ok(LazyUpgradePreparation::Skip),
            },
        }
    }

    fn spawn_lazy_upgrade_owner(&self, key: String, slot: Arc<LazyUpgradeSlot>) {
        let registry = self.clone_for_background();
        tokio::spawn(async move {
            let result = std::panic::AssertUnwindSafe(
                registry.run_lazy_upgrade_owner(key.clone(), slot.clone()),
            )
            .catch_unwind()
            .await;
            if result.is_err() {
                registry.recover_lazy_upgrade_owner_panic(key, slot).await;
            }
        });
    }

    async fn publish_connected_state(
        &self,
        key: &str,
        config: &McpServerConfig,
        discovery: &LiveDiscovery,
        retired_connection_id: Option<McpConnectionId>,
    ) {
        let _ = self
            .publish_connected_state_with_guard(key, config, discovery, retired_connection_id, None)
            .await;
    }

    async fn publish_connected_state_with_guard(
        &self,
        key: &str,
        config: &McpServerConfig,
        discovery: &LiveDiscovery,
        retired_connection_id: Option<McpConnectionId>,
        operation_guard: Option<&McpOperationGuard>,
    ) -> Result<(), McpError> {
        let mut conns = self.connections.write().await;
        #[cfg(test)]
        self.maybe_pause_before_client_publish().await;
        let mut clients = self.clients.write().await;
        let mut prompt_predecessors = self.prompt_predecessors.write().await;
        if operation_guard.is_some_and(|guard| !guard()) {
            return Err(operation_guard_rejected());
        }
        let (tools, resources, prompts) = match conns.get(key) {
            Some(McpConnectionState::Connected {
                tools,
                resources,
                prompts,
                ..
            })
            | Some(McpConnectionState::Cached {
                tools,
                resources,
                prompts,
                ..
            }) => (
                if discovery.catalog_failures.tools {
                    tools.clone()
                } else {
                    discovery.tools.clone()
                },
                if discovery.catalog_failures.resources {
                    resources.clone()
                } else {
                    discovery.resources.clone()
                },
                if discovery.catalog_failures.prompts {
                    prompts.clone()
                } else {
                    discovery.prompts.clone()
                },
            ),
            _ => (
                discovery.tools.clone(),
                discovery.resources.clone(),
                discovery.prompts.clone(),
            ),
        };
        conns.insert(
            key.to_string(),
            McpConnectionState::Connected {
                config: config.clone(),
                connection_id: discovery.connection_id,
                capabilities: discovery.capabilities.clone(),
                negotiated: discovery.negotiated.clone(),
                tools,
                resources,
                resource_templates: discovery.resource_templates.clone(),
                prompts,
                connected_at: SystemTime::now(),
            },
        );
        if let Some(client) = discovery.client.clone() {
            clients.insert(
                key.to_string(),
                RegisteredClient {
                    connection_id: Some(discovery.connection_id),
                    client,
                },
            );
        }
        prompt_predecessors.retain(|_, predecessor| predecessor.key != key);
        if let Some(cached_connection_id) = retired_connection_id {
            prompt_predecessors.insert(
                cached_connection_id,
                PromptPredecessor {
                    key: key.to_string(),
                    config: config.clone(),
                    live_connection_id: discovery.connection_id,
                },
            );
        }
        Ok(())
    }

    async fn discover_live_connection(
        &self,
        config: &McpServerConfig,
        negotiation_mode: crate::protocol_negotiation::NegotiationMode,
    ) -> Result<LiveDiscovery, McpError> {
        let connect_timeout = mcp_connection_timeout();
        let connect_started = std::time::Instant::now();
        let has_user_auth_header = crate::negotiation::spec_has_authorization(&config.spec);
        let helper_enabled = crate::headers_helper::has_headers_helper(&config.spec);
        let mut resolved_config = config.clone();
        let plugin_root = self
            .headers_helper_plugin_roots
            .read()
            .await
            .get(&config.name)
            .cloned();
        let (helper_spec, mut helper_minted_authorization) =
            crate::headers_helper::resolve_headers_helper_in(
                config,
                &self.headers_helper_cwd,
                plugin_root.as_deref(),
            )
            .await?;
        resolved_config.spec = helper_spec;
        let (connect_spec, oauth_key, mut grant_provenance) =
            if has_user_auth_header || helper_minted_authorization {
                (
                    resolved_config.spec.clone(),
                    None,
                    Some(GrantProvenance::unbound()),
                )
            } else {
                self.resolve_oauth_spec(&resolved_config).await?
            };

        tracing::debug!(
            server = %config.name,
            mode = ?negotiation_mode,
            "MCP protocol-era negotiation resolved"
        );

        let attempt = |spec: McpTransportSpec| {
            self.connect_attempt(spec, connect_timeout, &config.name, negotiation_mode)
        };

        let (conn, caps, negotiated) = match attempt(connect_spec.clone()).await {
            Ok(pair) => pair,
            Err(e) if oauth_key.is_some() => {
                let resource_metadata_url = error_resource_metadata_url(&e);
                if let Some(scope) = error_is_403_insufficient_scope(&e) {
                    let (stepped, stepped_grant) = self
                        .step_up_oauth_spec(
                            &resolved_config,
                            &scope,
                            resource_metadata_url.as_deref(),
                        )
                        .await?;
                    grant_provenance = stepped_grant;
                    attempt(stepped).await.map_err(|e| {
                        crate::negotiation::classify_auth_failure(
                            e,
                            has_user_auth_header,
                            helper_minted_authorization,
                        )
                    })?
                } else if error_is_401(&e) {
                    let (refreshed, refreshed_grant) = self
                        .reauth_oauth_spec(&resolved_config, resource_metadata_url.as_deref())
                        .await?;
                    grant_provenance = refreshed_grant;
                    attempt(refreshed).await.map_err(|e| {
                        crate::negotiation::classify_auth_failure(
                            e,
                            has_user_auth_header,
                            helper_minted_authorization,
                        )
                    })?
                } else {
                    return Err(crate::negotiation::classify_auth_failure(
                        e,
                        has_user_auth_header,
                        helper_minted_authorization,
                    ));
                }
            }
            Err(e) if helper_enabled && error_is_auth_response(&e) => {
                let (refreshed, minted) = crate::headers_helper::resolve_headers_helper_in(
                    config,
                    &self.headers_helper_cwd,
                    plugin_root.as_deref(),
                )
                .await?;
                helper_minted_authorization = minted;
                attempt(refreshed).await.map_err(|e| {
                    crate::negotiation::classify_auth_failure(
                        e,
                        has_user_auth_header,
                        helper_minted_authorization,
                    )
                })?
            }
            Err(e) => {
                return Err(crate::negotiation::classify_auth_failure(
                    e,
                    has_user_auth_header,
                    helper_minted_authorization,
                ))
            }
        };
        // Capture the grant-bound partition immediately after the authenticated
        // transport is established and before any catalog RPC. The write path
        // re-resolves it and refuses a mismatch, so a concurrent refresh-token
        // rotation cannot bind results fetched under the old grant to the new
        // cache partition.
        let discovery_cache_partition = if self.discovery_cache_store.is_some()
            && crate::discovery_cache::cache_gate_with_metadata(
                &config.spec,
                config.discovery_cache,
                crate::discovery_cache::feature_enabled(),
                &config.metadata,
            )
            .is_none()
        {
            self.discovery_cache_partition_for_grant(
                config,
                negotiation_mode,
                grant_provenance.as_ref(),
            )
            .await
            .ok()
        } else {
            None
        };
        let connection_duration_ms = connect_started.elapsed().as_millis() as u64;
        emit_server_connection_succeeded(&server_connection_succeeded_payload(
            config,
            connection_duration_ms,
            negotiation_mode,
            &negotiated,
        ));
        match std::panic::AssertUnwindSafe(async {
            let mut tools_list_elapsed = std::time::Duration::ZERO;
            let mut catalog_failures = CatalogFetchFailures::default();
            let mut tools = if caps.tools {
                let started = std::time::Instant::now();
                match self.transport.list_tools(&conn).await {
                    Ok(listed) => {
                        tools_list_elapsed = started.elapsed();
                        listed
                    }
                    Err(error) => {
                        catalog_failures.tools = true;
                        tracing::warn!(
                            server = %config.name,
                            %error,
                            "Failed to fetch tools catalog"
                        );
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            let resources = if caps.resources {
                match self.transport.list_resources(&conn).await {
                    Ok(listed) => listed,
                    Err(error) => {
                        catalog_failures.resources = true;
                        tracing::warn!(
                            server = %config.name,
                            %error,
                            "Failed to fetch resources catalog"
                        );
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            let templates_eligible = crate::discovery_cache::cache_gate_with_metadata(
                &config.spec,
                config.discovery_cache,
                crate::discovery_cache::feature_enabled(),
                &config.metadata,
            )
            .is_none();
            let resource_templates = if caps.resources && templates_eligible {
                match self.transport.list_resource_templates(&conn).await {
                    Ok(templates) => {
                        emit_resource_templates_fetched(&resource_templates_fetched_payload(
                            &templates,
                        ));
                        templates
                    }
                    Err(error) => {
                        tracing::warn!(
                            server = %config.name,
                            %error,
                            "Failed to fetch resource templates"
                        );
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            let prompts = if caps.prompts {
                match self.transport.list_prompts(&conn).await {
                    Ok(listed) => listed,
                    Err(error) => {
                        catalog_failures.prompts = true;
                        tracing::warn!(
                            server = %config.name,
                            %error,
                            "Failed to fetch prompts catalog"
                        );
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };

            let normalized_server = normalize_name_for_mcp(&config.name);
            let gate_url = {
                let u = spec_url(&config.spec);
                (!u.is_empty()).then(|| u.to_string())
            };
            let server_display = config.name.clone();
            let mut degraded_counts: std::collections::HashMap<
                telemetry::tengu::mcp::DegradedReason,
                u32,
            > = std::collections::HashMap::new();
            if catalog_failures.tools {
                degraded_counts.insert(telemetry::tengu::mcp::DegradedReason::ToolsListFailed, 1);
            }
            if catalog_failures.resources {
                degraded_counts.insert(
                    telemetry::tengu::mcp::DegradedReason::ResourcesListFailed,
                    1,
                );
            }
            if catalog_failures.prompts {
                degraded_counts.insert(
                    telemetry::tengu::mcp::DegradedReason::PromptsListFailed,
                    1,
                );
            }
            if !catalog_failures.tools && connected_zero_tools_fires(caps.tools, tools.len()) {
                degraded_counts.insert(
                    telemetry::tengu::mcp::DegradedReason::ConnectedZeroTools,
                    1,
                );
            }
            tools.retain_mut(|dto| {
                let decision = crate::tool_schema::decide_tool_schema(
                    gate_url.as_deref(),
                    &dto.input_schema,
                );
                if decision.normalized {
                    *degraded_counts
                        .entry(telemetry::tengu::mcp::DegradedReason::ToolSchemaNormalized)
                        .or_insert(0) += 1;
                }
                if let Some(reason) = decision.classification {
                    *degraded_counts.entry(reason).or_insert(0) += 1;
                }
                if let Some(reason) = decision.drop_reason {
                    tracing::warn!(
                        server = %server_display,
                        tool = %dto.tool_name,
                        "Skipping tool \"{}\": {reason}. Other tools from this server remain available.",
                        dto.tool_name
                    );
                    return false;
                }
                if let Some(warning) = &decision.warning {
                    tracing::debug!(
                        server = %server_display,
                        tool = %dto.tool_name,
                        "Tool \"{}\" {warning}",
                        dto.tool_name
                    );
                }
                if let Some(note) = decision.description_note {
                    dto.description = if dto.description.is_empty() {
                        note
                    } else {
                        format!("{note}\n\n{}", dto.description)
                    };
                }
                dto.input_schema = decision.schema;
                dto.server_name.clone_from(&server_display);
                dto.full_name = format!(
                    "mcp__{}__{}",
                    normalized_server,
                    normalize_name_for_mcp(&dto.tool_name)
                );
                true
            });

            if caps.tools && !catalog_failures.tools {
                emit_tools_listed(&tools_listed_payload(
                    config.spec.kind(),
                    tools_list_elapsed,
                    &tools,
                    &server_display,
                ));
            }
            for payload in
                degraded_payloads_for_server(&degraded_counts, config.spec.kind(), &server_display)
            {
                emit_degraded(&payload);
            }

            let connection_id = conn.connection_id;
            let mut client = None;
            let mut listener_connection = None;
            if let Some(raw_conn) = &self.raw_conn {
                if let Some(connection) = raw_conn.connection_for(connection_id) {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    client = Some(Arc::new(
                        McpClient::with_roots(
                            config.name.clone(),
                            cwd,
                            self.additional_roots.clone(),
                            connection.clone(),
                            self.hook_dispatcher.clone(),
                        )
                        .await
                        .with_config_options(
                            config.timeout_ms,
                            config.always_load,
                            config.tools.clone(),
                            config.tool_permissions.clone(),
                        )
                        .with_transport_kind(config.spec.transport_kind())
                        .with_negotiated_protocol(negotiated.clone())
                        .with_server_url(gate_url.clone()),
                    ));
                    listener_connection = Some(connection);
                }
            }

            LiveDiscovery {
                connection_id,
                connection_duration_ms,
                negotiated,
                negotiation_mode,
                grant_provenance,
                capabilities: caps,
                tools,
                resources,
                resource_templates,
                prompts,
                catalog_failures,
                discovery_cache_partition,
                client,
                listener_connection,
            }
        })
        .catch_unwind()
        .await
        {
            Ok(discovery) => Ok(discovery),
            Err(_) => {
                self.disconnect_or_schedule_cleanup(conn.connection_id)
                    .await;
                Err(lazy_upgrade_panic_error(
                    &config.name,
                    "post-connect discovery",
                ))
            }
        }
    }

    async fn run_lazy_upgrade_owner(&self, key: String, slot: Arc<LazyUpgradeSlot>) {
        if let Err(error) = Self::validate_connectable_config(&slot.expected_config) {
            let terminal = {
                let lifecycle = self.lifecycle_lock(&key);
                let _guard = lifecycle.lock().await;
                self.finish_lazy_upgrade_failure_locked(&key, &slot, &error)
                    .await
            };
            slot.finish_if_unset(terminal);
            return;
        }

        let discovery = match self
            .discover_live_connection(&slot.expected_config, slot.negotiation_mode)
            .await
        {
            Ok(discovery) => discovery,
            Err(error) => {
                let terminal = {
                    let lifecycle = self.lifecycle_lock(&key);
                    let _guard = lifecycle.lock().await;
                    self.finish_lazy_upgrade_failure_locked(&key, &slot, &error)
                        .await
                };
                slot.finish_if_unset(terminal);
                return;
            }
        };

        let (terminal, cleanup) = {
            let lifecycle = self.lifecycle_lock(&key);
            let _guard = lifecycle.lock().await;
            match self
                .install_lazy_upgrade_live_discovery_if_current(&key, &slot, discovery)
                .await
            {
                BackgroundInstallOutcome::Installed(connection_id) => {
                    self.remove_lazy_upgrade_slot_if_matches(&key, &slot).await;
                    (LazyUpgradeTerminal::Success(connection_id), None)
                }
                BackgroundInstallOutcome::Rejected(discovery) => {
                    self.remove_lazy_upgrade_slot_if_matches(&key, &slot).await;
                    (
                        LazyUpgradeTerminal::Error(Arc::new(slot.stale_error())),
                        Some(discovery),
                    )
                }
            }
        };
        if let Some(discovery) = cleanup {
            self.discard_live_discovery(discovery).await;
        }
        slot.finish_if_unset(terminal);
    }

    async fn recover_lazy_upgrade_owner_panic(&self, key: String, slot: Arc<LazyUpgradeSlot>) {
        let terminal = {
            let lifecycle = self.lifecycle_lock(&key);
            let _guard = lifecycle.lock().await;
            let terminal = self
                .finish_lazy_upgrade_failure_locked(&key, &slot, &slot.panic_error())
                .await;
            self.remove_lazy_upgrade_slot_if_matches(&key, &slot).await;
            terminal
        };
        slot.finish_if_unset(terminal);
    }

    async fn install_live_discovery(
        &self,
        key: String,
        config: McpServerConfig,
        discovery: LiveDiscovery,
        retired_connection_id: Option<McpConnectionId>,
        operation_guard: Option<&McpOperationGuard>,
        modern_open_telemetry: Option<ModernListenOpenTelemetry>,
    ) -> Result<McpConnectionId, McpError> {
        let server_name = config.name.clone();
        let connection_id = discovery.connection_id;
        let shared_server = key == server_name;
        let capabilities = discovery.capabilities.clone();
        let listener_connection = discovery.listener_connection.clone();
        if let Err(error) = self
            .publish_connected_state_with_guard(
                &key,
                &config,
                &discovery,
                retired_connection_id,
                operation_guard,
            )
            .await
        {
            self.discard_live_discovery(discovery).await;
            return Err(error);
        }
        let (tools, resources, resource_templates, prompts) = self
            .published_catalogs_for_connection(&key, connection_id, &discovery)
            .await;
        self.persist_or_purge_discovery_cache(
            &config,
            discovery.discovery_cache_partition.as_ref(),
            &capabilities,
            &tools,
            &resources,
            &resource_templates,
            &prompts,
            discovery.negotiation_mode,
            discovery.grant_provenance.as_ref(),
            Some(&discovery.negotiated),
        )
        .await;
        if let Some(connection) = listener_connection {
            if shared_server {
                self.spawn_catalog_change_listener(
                    server_name.clone(),
                    connection_id,
                    connection,
                    discovery.negotiated.clone(),
                    capabilities.clone(),
                    modern_open_telemetry,
                );
            }
        }
        if shared_server {
            if modern_open_telemetry.is_some() {
                self.publish_listener_reopen_catalog_changes(
                    &server_name,
                    connection_id,
                    retired_connection_id,
                    &capabilities,
                )
                .await;
            } else {
                self.publish_catalog_change(McpCatalogChanged {
                    server_name,
                    connection_id,
                    retired_connection_id,
                    kind: McpCatalogKind::Tools,
                    telemetry_cause: None,
                })
                .await;
            }
        }
        Ok(connection_id)
    }

    async fn install_lazy_upgrade_live_discovery_if_current(
        &self,
        key: &str,
        slot: &Arc<LazyUpgradeSlot>,
        discovery: LiveDiscovery,
    ) -> BackgroundInstallOutcome {
        let server_name = slot.expected_config.name.clone();
        let shared_server = key == server_name;
        let capabilities = discovery.capabilities.clone();
        let listener_connection = discovery.listener_connection.clone();
        let current_slot = self.lazy_upgrade_slots.read().await.get(key).cloned();
        let installed = {
            if !matches!(current_slot.as_ref(), Some(current_slot) if Arc::ptr_eq(current_slot, slot))
            {
                false
            } else {
                let expected_current = match slot.mode {
                    LazyUpgradeMode::Foreground => matches!(
                        self.connections.read().await.get(key),
                        Some(McpConnectionState::Connecting { config, .. })
                            if Self::same_config_snapshot(config, &slot.expected_config)
                    ),
                    LazyUpgradeMode::Background => matches!(
                        self.connections.read().await.get(key),
                        Some(McpConnectionState::Cached {
                            connection_id,
                            config,
                            ..
                        }) if *connection_id == slot.cached_connection_id
                            && Self::same_config_snapshot(config, &slot.expected_config)
                    ),
                };
                if expected_current {
                    let expected_era = match slot.negotiation_mode {
                        crate::protocol_negotiation::NegotiationMode::Auto { .. } => "modern",
                        crate::protocol_negotiation::NegotiationMode::Legacy => "legacy",
                    };
                    let expected_changed =
                        slot.refresh_partition.as_ref().is_some_and(|partition| {
                            partition.negotiation_mode != slot.negotiation_mode
                                || partition.expected_era != expected_era
                        }) || discovery.negotiation_mode != slot.negotiation_mode;
                    let grant_changed = slot.refresh_partition.as_ref().is_some_and(|partition| {
                        discovery
                            .discovery_cache_partition
                            .as_ref()
                            .is_none_or(|live| live.partition_key != partition.partition_key)
                    });
                    let era_changed = slot.mode == LazyUpgradeMode::Background
                        && slot.refresh_partition.is_some()
                        && slot.refresh_entry_era.as_deref().unwrap_or("legacy")
                            != negotiated_era_label(discovery.negotiated.era);
                    if expected_changed || grant_changed || era_changed {
                        if let (Some(store), Some(partition)) =
                            (&self.discovery_cache_store, slot.refresh_partition.as_ref())
                        {
                            if let Err(error) = store.purge_partitioned(&partition.partition_key) {
                                tracing::warn!(
                                    server = %slot.expected_config.name,
                                    partition = %partition.partition_key,
                                    %error,
                                    "Discovery cache stale partition purge skipped after protocol-era change"
                                );
                            }
                        }
                        false
                    } else {
                        self.publish_connected_state(
                            key,
                            &slot.expected_config,
                            &discovery,
                            Some(slot.cached_connection_id),
                        )
                        .await;
                        true
                    }
                } else {
                    false
                }
            }
        };
        if !installed {
            return BackgroundInstallOutcome::Rejected(discovery);
        }
        let (tools, resources, resource_templates, prompts) = self
            .published_catalogs_for_connection(key, discovery.connection_id, &discovery)
            .await;
        self.persist_or_purge_discovery_cache(
            &slot.expected_config,
            discovery.discovery_cache_partition.as_ref(),
            &capabilities,
            &tools,
            &resources,
            &resource_templates,
            &prompts,
            slot.negotiation_mode,
            discovery.grant_provenance.as_ref(),
            Some(&discovery.negotiated),
        )
        .await;
        if let Some(connection) = listener_connection {
            if shared_server {
                self.spawn_catalog_change_listener(
                    server_name.clone(),
                    discovery.connection_id,
                    connection,
                    discovery.negotiated.clone(),
                    capabilities.clone(),
                    None,
                );
            }
        }
        if shared_server {
            self.publish_catalog_change(McpCatalogChanged {
                server_name,
                connection_id: discovery.connection_id,
                retired_connection_id: Some(slot.cached_connection_id),
                kind: McpCatalogKind::Tools,
                telemetry_cause: None,
            })
            .await;
        }
        BackgroundInstallOutcome::Installed(discovery.connection_id)
    }

    async fn published_catalogs_for_connection(
        &self,
        key: &str,
        connection_id: McpConnectionId,
        discovery: &LiveDiscovery,
    ) -> (
        Vec<lingxi_core::host::McpToolDto>,
        Vec<lingxi_core::host::McpResourceDto>,
        Vec<lingxi_core::host::McpResourceTemplateDto>,
        Vec<lingxi_core::host::McpPromptDto>,
    ) {
        let conns = self.connections.read().await;
        match conns.get(key) {
            Some(McpConnectionState::Connected {
                connection_id: current,
                tools,
                resources,
                resource_templates,
                prompts,
                ..
            }) if *current == connection_id => (
                tools.clone(),
                resources.clone(),
                resource_templates.clone(),
                prompts.clone(),
            ),
            _ => (
                discovery.tools.clone(),
                discovery.resources.clone(),
                discovery.resource_templates.clone(),
                discovery.prompts.clone(),
            ),
        }
    }

    async fn finish_lazy_upgrade_failure_locked(
        &self,
        key: &str,
        slot: &Arc<LazyUpgradeSlot>,
        error: &McpError,
    ) -> LazyUpgradeTerminal {
        let current_slot = self.lazy_upgrade_slots.read().await.get(key).cloned();
        let still_current =
            matches!(current_slot.as_ref(), Some(current_slot) if Arc::ptr_eq(current_slot, slot));
        if still_current {
            match slot.mode {
                LazyUpgradeMode::Foreground => {
                    let current_connecting = matches!(
                        self.connections.read().await.get(key),
                        Some(McpConnectionState::Connecting { config, .. })
                            if Self::same_config_snapshot(config, &slot.expected_config)
                    );
                    if current_connecting {
                        self.connections.write().await.insert(
                            key.to_string(),
                            McpConnectionState::Disconnected {
                                config: slot.expected_config.clone(),
                                last_error: Some(error.to_string()),
                            },
                        );
                        self.clear_prompt_predecessors_for_key(key).await;
                        self.emit_retire_event_if_shared(
                            &slot.expected_config,
                            key,
                            slot.cached_connection_id,
                        )
                        .await;
                    }
                }
                LazyUpgradeMode::Background => {
                    self.record_discovery_cache_refresh_failure_locked(
                        key,
                        slot.cached_connection_id,
                        &slot.expected_config,
                        slot.refresh_partition.as_ref(),
                    )
                    .await;
                }
            }
            self.remove_lazy_upgrade_slot_if_matches(key, slot).await;
        }
        LazyUpgradeTerminal::Error(Arc::new(clone_mcp_error(error)))
    }

    async fn discard_live_discovery(&self, discovery: LiveDiscovery) {
        self.disconnect_or_schedule_cleanup(discovery.connection_id)
            .await;
    }

    async fn emit_retire_event_if_shared(
        &self,
        config: &McpServerConfig,
        key: &str,
        connection_id: McpConnectionId,
    ) {
        if key != config.name {
            return;
        }
        self.publish_catalog_change(McpCatalogChanged {
            server_name: config.name.clone(),
            connection_id,
            retired_connection_id: Some(connection_id),
            kind: McpCatalogKind::Tools,
            telemetry_cause: None,
        })
        .await;
    }

    async fn disconnect_for_cleanup(&self, connection_id: McpConnectionId) -> Result<(), McpError> {
        tokio::time::timeout(
            cleanup_disconnect_timeout(),
            self.transport.disconnect(connection_id),
        )
        .await
        .map_err(|_| {
            McpError::Internal(format!(
                "disconnect timed out after {:?}",
                cleanup_disconnect_timeout()
            ))
        })?
    }

    async fn disconnect_or_schedule_cleanup(&self, connection_id: McpConnectionId) {
        if self.disconnect_for_cleanup(connection_id).await.is_err() {
            self.schedule_transport_cleanup_retry(connection_id).await;
        } else {
            self.pending_transport_cleanups
                .write()
                .await
                .remove(&connection_id);
        }
    }

    async fn schedule_transport_cleanup_retry(&self, connection_id: McpConnectionId) {
        let should_spawn = {
            let mut pending = self.pending_transport_cleanups.write().await;
            match pending.get_mut(&connection_id) {
                Some(entry) if entry.retrying => false,
                Some(entry) => {
                    entry.retrying = true;
                    true
                }
                None => {
                    pending.insert(connection_id, PendingTransportCleanup { retrying: true });
                    true
                }
            }
        };
        if !should_spawn {
            return;
        }
        let registry = self.clone_for_background();
        tokio::spawn(async move {
            registry
                .retry_pending_transport_cleanup(connection_id)
                .await;
        });
    }

    async fn retry_pending_transport_cleanup(&self, connection_id: McpConnectionId) {
        const MAX_RETRIES: u8 = 5;
        let mut delay = Duration::from_millis(10);
        for attempt in 0..MAX_RETRIES {
            tokio::time::sleep(delay).await;
            match self.disconnect_for_cleanup(connection_id).await {
                Ok(()) => {
                    self.pending_transport_cleanups
                        .write()
                        .await
                        .remove(&connection_id);
                    return;
                }
                Err(error) => {
                    tracing::warn!(
                        connection_id = %connection_id,
                        %error,
                        "retrying MCP transport cleanup after disconnect failure"
                    );
                    if attempt + 1 == MAX_RETRIES {
                        break;
                    }
                    delay = std::cmp::min(delay.saturating_mul(2), Duration::from_millis(250));
                }
            }
        }
        if let Some(entry) = self
            .pending_transport_cleanups
            .write()
            .await
            .get_mut(&connection_id)
        {
            entry.retrying = false;
        }
    }

    async fn kick_pending_transport_cleanups(&self) {
        let pending: Vec<McpConnectionId> = {
            let pending = self.pending_transport_cleanups.read().await;
            pending
                .iter()
                .filter_map(|(connection_id, entry)| (!entry.retrying).then_some(*connection_id))
                .collect()
        };
        for connection_id in pending {
            self.schedule_transport_cleanup_retry(connection_id).await;
        }
    }

    /// Connect and initialize under one deadline. If initialization fails or
    /// times out after a transport was opened, retire that transport before
    /// returning so callers never leak a live stdio child/socket.
    async fn connect_attempt(
        &self,
        spec: McpTransportSpec,
        timeout: Duration,
        server_name: &str,
        negotiation_mode: crate::protocol_negotiation::NegotiationMode,
    ) -> Result<
        (
            McpRawConnection,
            ServerCapabilitiesDto,
            lingxi_core::host::McpNegotiatedProtocol,
        ),
        McpError,
    > {
        let deadline = tokio::time::Instant::now() + timeout;
        let timeout_error = || {
            McpError::Connection(format!(
                "MCP server \"{server_name}\" connection timed out after {}ms",
                timeout.as_millis()
            ))
        };
        let expected_era = match negotiation_mode {
            crate::protocol_negotiation::NegotiationMode::Auto { .. } => {
                lingxi_core::host::McpProtocolEra::Modern
            }
            crate::protocol_negotiation::NegotiationMode::Legacy => {
                lingxi_core::host::McpProtocolEra::Legacy
            }
        };
        let probe_timeout_ms = match negotiation_mode {
            crate::protocol_negotiation::NegotiationMode::Auto { probe_timeout_ms } => {
                Some(probe_timeout_ms)
            }
            crate::protocol_negotiation::NegotiationMode::Legacy => None,
        };
        match std::panic::AssertUnwindSafe(async {
            tokio::time::timeout_at(
                deadline,
                self.transport.connect_and_initialize(
                    &spec,
                    lingxi_core::host::McpConnectOptions {
                        expected_era: Some(expected_era),
                        deadline_ms: timeout.as_millis() as u64,
                        probe_timeout_ms,
                    },
                ),
            )
            .await
        })
        .catch_unwind()
        .await
        {
            Ok(Ok(Ok(result))) => Ok((result.connection, result.capabilities, result.negotiated)),
            Ok(Ok(Err(error))) => Err(error),
            Ok(Err(_)) => Err(timeout_error()),
            Err(payload) if panic_payload_mentions_connect(payload.as_ref()) => {
                // A panic before a raw connection is returned belongs to the
                // detached lazy-upgrade owner, which records the terminal
                // "cached lazy-upgrade task panicked" error. Initialize
                // panics, in contrast, have a known connection and retain
                // the existing phase-specific error/cleanup behavior.
                std::panic::resume_unwind(payload);
            }
            Err(_) => Err(lazy_upgrade_panic_error(server_name, "initialize")),
        }
    }

    /// Connect every server in `configs` at startup, mirroring claude-code's
    /// `loadAndConnectMcpConfigs` (services/mcp/useManageMCPConnections.ts).
    ///
    /// For each config:
    /// - **Disabled** servers (`config.disabled`) are seeded as
    ///   `Disconnected { last_error: None }` and skipped — `/mcp` still lists
    ///   them but they never auto-connect. This subsumes the manual
    ///   pre-population block in `apps/cli/src/init.rs`.
    /// - Otherwise the per-server lifecycle lock is taken and
    ///   [`Self::connect_locked`] is invoked directly, so the same generation-
    ///   safe state install/writeback logic covers startup auto-connect,
    ///   reconnect, and enable flows.
    ///
    /// One server's failure never aborts the batch; per-server failures are
    /// logged via `tracing::warn!`. Returns the per-server outcome in the same
    /// order as `configs`.
    pub async fn connect_all(
        &self,
        configs: Vec<McpServerConfig>,
    ) -> Vec<(String, Result<McpConnectionId, McpError>)> {
        use futures_util::future::join_all;
        self.freeze_configuration();
        let futs = configs.into_iter().map(|config| async move {
            let name = config.name.clone();
            let lifecycle = self.lifecycle_lock(&name);
            let _guard = lifecycle.lock().await;
            if config.disabled {
                let mut conns = self.connections.write().await;
                match conns.get(&name) {
                    Some(McpConnectionState::Connected { .. })
                    | Some(McpConnectionState::Cached { .. })
                    | Some(McpConnectionState::HealthChecking { .. })
                    | Some(McpConnectionState::Connecting { .. })
                    | Some(McpConnectionState::AwaitingOAuth { .. })
                    | Some(McpConnectionState::Reconnecting { .. }) => {}
                    _ => {
                        conns.insert(
                            name.clone(),
                            McpConnectionState::Disconnected {
                                config,
                                last_error: None,
                            },
                        );
                    }
                }
                tracing::debug!(server = %name, "skipping disabled MCP server");
                return None;
            }

            let result = self.connect_locked(config, None).await;
            if let Err(ref e) = result {
                tracing::warn!(server = %name, error = %e, "MCP auto-connect failed");
            }
            Some((name, result))
        });
        join_all(futs).await.into_iter().flatten().collect()
    }

    /// Background reconnect/backoff loop (claude-code `reconnectWithBackoff`).
    ///
    /// Spawn with `tokio::spawn(registry.clone().run_reconnect_loop())`. The
    /// loop periodically scans `connections` for servers eligible to retry —
    /// `Disconnected { last_error: Some(_) }` (a failed connect) or
    /// `Reconnecting { .. }` (a retry already in flight) — and drives each
    /// through [`Self::reconnect_one`].
    ///
    /// Per claude-code, stdio servers are NOT auto-reconnected (a dead local
    /// process won't recover on its own); only remote transports are enrolled.
    pub async fn run_reconnect_loop(self: Arc<Self>) {
        loop {
            self.kick_pending_transport_cleanups().await;
            let candidates: Vec<McpServerConfig> = {
                let conns = self.connections.read().await;
                conns
                    .values()
                    .filter_map(|state| match state {
                        McpConnectionState::Disconnected {
                            config,
                            last_error: Some(_),
                        }
                        | McpConnectionState::Reconnecting { config, .. } => Some(config.clone()),
                        _ => None,
                    })
                    .filter(|config| !config.disabled && config.spec.kind() != "stdio")
                    .collect()
            };

            for config in candidates {
                Arc::clone(&self).reconnect_one(config).await;
            }

            tokio::time::sleep(self.health_check_interval).await;
        }
    }

    /// Drive a single server through the reconnect/backoff schedule.
    ///
    /// Attempts `connect` up to `self.max_retry_count` times. The FIRST attempt
    /// fires immediately (no leading sleep); after a failed NON-final attempt
    /// the task sleeps `min(INITIAL_BACKOFF * 2^(attempt-1), MAX_BACKOFF)` and
    /// the final attempt has NO trailing sleep — matching claude-code
    /// `useManageMCPConnections.ts:372-461`. For the default 5 attempts the
    /// sleeps are 1s, 2s, 4s, 8s, so attempts fire at t = 0, 1, 3, 7, 15s (the
    /// 16s/30s-cap value is never used). While waiting, the server rests in
    /// `Reconnecting { retry_count, next_retry_at }`. On success the server is
    /// left `Connected` (set by [`Self::connect`]); after the final attempt
    /// fails it transitions to `Failed { error, attempts }`.
    ///
    /// Aborts without marking `Failed` if the server is concurrently `Stopped`
    /// or its config is flipped to `disabled` (claude-code disabled-mid-wait
    /// guard).
    async fn reconnect_one(self: Arc<Self>, config: McpServerConfig) {
        let name = config.name.clone();
        let max = self.max_retry_count.max(1);

        for attempt in 1..=max {
            // Everything except the inter-attempt backoff runs UNDER the
            // per-server lifecycle lock, so the guard-read + `Reconnecting`
            // insert + connect (+ the terminal `Failed` insert) form ONE
            // critical section that cannot interleave with a locked
            // set_disabled/disconnect/connect. Previously the guard-read and the
            // `Reconnecting` insert ran unlocked, and only the inner connect took
            // the lock — a TOCTOU that could overwrite a `Connected{connection_id}`
            // won by a concurrent connect, stranding that id (unreachable for
            // teardown = a leaked remote connection). `true` ⇒ back off + retry.
            let retry = {
                let lifecycle = self.lifecycle_lock(&name);
                let _guard = lifecycle.lock().await;

                match self.connections.read().await.get(&name) {
                    Some(McpConnectionState::Stopped { .. }) | None => {
                        tracing::debug!(server = %name, "reconnect aborted: server stopped");
                        return;
                    }
                    // A concurrent connect/reconnect already brought the server
                    // up — leave its live connection alone; overwriting it with
                    // `Reconnecting` would leak that connection id. §11 Stage 2:
                    // a concurrent connect that resolved `Cached` counts too —
                    // it already serves this server without a dial; the
                    // reconnect loop only exists to un-stick a broken one.
                    Some(
                        McpConnectionState::Connected { .. } | McpConnectionState::Cached { .. },
                    ) => {
                        tracing::debug!(server = %name, "reconnect aborted: already connected");
                        return;
                    }
                    Some(state) if state_is_disabled(state) => {
                        tracing::debug!(server = %name, "reconnect aborted: server disabled");
                        return;
                    }
                    _ => {}
                }

                // `next_retry_at` is the wall-clock time the NEXT attempt would
                // fire if this one fails; the final attempt has no successor, so
                // it points at the present.
                let next_retry_at = match post_attempt_backoff(attempt, max) {
                    Some(backoff) => SystemTime::now() + backoff,
                    None => SystemTime::now(),
                };
                self.connections.write().await.insert(
                    name.clone(),
                    McpConnectionState::Reconnecting {
                        config: config.clone(),
                        retry_count: attempt,
                        next_retry_at,
                    },
                );

                // claude-code runs attempt 1 IMMEDIATELY — there is NO leading
                // sleep before the first connect (`useManageMCPConnections.ts:372`).
                // `connect_locked` (NOT `connect`) — the lifecycle lock is already
                // held; `connect` would re-acquire it and deadlock.
                match self.connect_locked(config.clone(), None).await {
                    Ok(_) => {
                        tracing::info!(server = %name, attempt, "MCP reconnect succeeded");
                        return;
                    }
                    Err(e) if attempt == max => {
                        tracing::warn!(
                            server = %name,
                            attempts = attempt,
                            error = %e,
                            "MCP reconnect exhausted; marking Failed"
                        );
                        self.connections.write().await.insert(
                            name.clone(),
                            McpConnectionState::Failed {
                                config: config.clone(),
                                error: e.to_string(),
                                attempts: attempt,
                            },
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::debug!(
                            server = %name,
                            attempt,
                            error = %e,
                            "MCP reconnect attempt failed; backing off"
                        );
                        true
                    }
                }
            };

            // Back off ONLY after a failed NON-final attempt (lock released); the
            // final attempt has no trailing sleep (claude-code schedules the
            // *next* retry, never one after the last).
            if retry {
                if let Some(backoff) = post_attempt_backoff(attempt, max) {
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }

    /// Drop the named connection and transition it to `Stopped`.
    pub async fn disconnect(&self, name: &str) -> Result<(), McpError> {
        let lifecycle = self.lifecycle_lock(name);
        let _guard = lifecycle.lock().await;
        self.disconnect_locked_inner(name, true, false).await
    }

    async fn disconnect_locked(&self, name: &str) -> Result<(), McpError> {
        self.disconnect_locked_inner(name, true, false).await
    }

    async fn disconnect_locked_inner(
        &self,
        name: &str,
        revoke_oauth: bool,
        remove_state: bool,
    ) -> Result<(), McpError> {
        self.kick_pending_transport_cleanups().await;
        let invalidated_slot = self.invalidate_lazy_upgrade_slot(name).await;
        // §11 Stage 2 — a `Cached` server has NO live transport connection to
        // tear down (`is_live = false`): its `connection_id` is a synthetic
        // one minted purely to key the tool-registry partition (see
        // `McpConnectionState::Cached`'s doc), so calling
        // `self.transport.disconnect` on it would hand the platform transport
        // an id it never registered. It is still disconnect-able, though —
        // the user must be able to `/mcp disconnect` a cache-served server
        // exactly as they would a live one.
        let slot_owned_connecting = invalidated_slot.is_some();
        let Some((config, generation)) = ({
            let conns = self.connections.read().await;
            match conns.get(name) {
                Some(McpConnectionState::Connected {
                    connection_id,
                    config,
                    ..
                })
                | Some(McpConnectionState::HealthChecking {
                    connection_id,
                    config,
                }) => Some((config.clone(), Some((*connection_id, true)))),
                Some(McpConnectionState::Cached {
                    connection_id,
                    config,
                    ..
                }) => Some((config.clone(), Some((*connection_id, false)))),
                Some(McpConnectionState::Connecting { config, .. })
                    if slot_owned_connecting || remove_state =>
                {
                    Some((config.clone(), None))
                }
                Some(
                    McpConnectionState::Disconnected { config, .. }
                    | McpConnectionState::AwaitingOAuth { config, .. }
                    | McpConnectionState::Reconnecting { config, .. }
                    | McpConnectionState::Failed { config, .. }
                    | McpConnectionState::Stopped { config },
                ) if remove_state => Some((config.clone(), None)),
                _ => None,
            }
        }) else {
            return Ok(());
        };

        // Do not remove the state before the transport confirms teardown. If
        // teardown fails, callers keep the still-live state and cached client.
        // The per-server lifecycle lock prevents a concurrent connect from
        // racing this await, while snapshots for every server remain unblocked.
        if let Some((connection_id, true)) = generation {
            self.transport.disconnect(connection_id).await?;
        }

        let transitioned = {
            let mut conns = self.connections.write().await;
            let same_generation = match conns.get(name) {
                Some(McpConnectionState::Connected {
                    connection_id: current,
                    ..
                })
                | Some(McpConnectionState::HealthChecking {
                    connection_id: current,
                    ..
                }) => {
                    matches!(generation, Some((connection_id, true)) if *current == connection_id)
                }
                Some(McpConnectionState::Cached {
                    connection_id: current,
                    ..
                }) => {
                    matches!(generation, Some((connection_id, false)) if *current == connection_id)
                }
                Some(
                    McpConnectionState::Disconnected {
                        config: current, ..
                    }
                    | McpConnectionState::Connecting {
                        config: current, ..
                    }
                    | McpConnectionState::AwaitingOAuth {
                        config: current, ..
                    }
                    | McpConnectionState::Reconnecting {
                        config: current, ..
                    }
                    | McpConnectionState::Failed {
                        config: current, ..
                    }
                    | McpConnectionState::Stopped { config: current },
                ) => generation.is_none() && Self::same_config_snapshot(current, &config),
                _ => false,
            };
            if same_generation {
                if remove_state {
                    conns.remove(name);
                } else {
                    conns.insert(
                        name.to_string(),
                        McpConnectionState::Stopped {
                            config: config.clone(),
                        },
                    );
                }
            }
            same_generation
        };
        if !transitioned {
            return Ok(());
        }

        // Drop any cached `McpClient` so `get_client(name)` stops returning a
        // handle to the now-dead connection. `shift_remove` preserves discovery
        // order for the remaining entries.
        self.clients.write().await.shift_remove(name);
        self.clear_prompt_predecessors_for_key(name).await;
        Self::finish_invalidated_lazy_upgrade_slot(invalidated_slot.as_ref());
        // Remove this connection's dynamic tool partition immediately. A later
        // reconnect emits a fresh Tools event with its new connection id.
        if let Some((connection_id, _)) = generation {
            self.emit_retire_event_if_shared(&config, name, connection_id)
                .await;
        }

        // A lifecycle removal must not let the same config/grant immediately
        // resurrect a retired catalog. Plugin unload uses the same purge while
        // deliberately retaining its OAuth row (`revoke_oauth == false`).
        if let Some(store) = &self.discovery_cache_store {
            if let Err(error) = store.purge_server_family(&config.name) {
                tracing::warn!(
                    server = %config.name,
                    %error,
                    "Discovery cache lifecycle purge skipped"
                );
            }
        }

        // Token revocation (RFC 7009) is best-effort and intentionally runs
        // after local state/catalog retirement, so slow network I/O cannot make
        // a dead server continue to appear live.
        if revoke_oauth {
            if let (Some(deps), Some(oauth_cfg)) = (self.oauth.as_ref(), spec_oauth(&config.spec)) {
                let key = oauth::server_key(&config.name, &config.spec);
                oauth::revoke_server_tokens(
                    &deps.storage,
                    &deps.http,
                    &key,
                    spec_url(&config.spec),
                    oauth_cfg,
                )
                .await;
            }
        }
        Ok(())
    }

    /// Remove a configured server and its cached client from the registry.
    ///
    /// This is intentionally separate from `disconnect`: a disconnected
    /// server remains visible in `/mcp`, while a configuration reload must
    /// remove entries deleted from the on-disk config as well.
    pub async fn remove(&self, name: &str) -> Result<(), McpError> {
        let lifecycle = self.lifecycle_lock(name);
        let _guard = lifecycle.lock().await;
        self.disconnect_locked_inner(name, true, true).await
    }

    /// Lifecycle-safe removal variant for plugin unload: retire any live or
    /// cached partition and drop the state/client without revoking stored auth.
    pub async fn remove_without_revoking_auth(&self, name: &str) -> Result<(), McpError> {
        let lifecycle = self.lifecycle_lock(name);
        let _guard = lifecycle.lock().await;
        self.disconnect_locked_inner(name, false, true).await
    }

    /// Remove a configured server only when its current serialized config still
    /// matches `expected`. The comparison happens while holding the per-server
    /// lifecycle lock, so an asynchronous settings reload cannot retire a
    /// newer generation after waiting behind a pending connect or OAuth flow.
    /// This preserves the full client/catalog/lazy-slot/cache-retire cleanup
    /// while intentionally retaining the OAuth grant.
    pub async fn remove_without_revoking_auth_if_config(
        &self,
        name: &str,
        expected: &McpServerConfig,
    ) -> Result<bool, McpError> {
        self.remove_without_revoking_auth_if_config_with_guard(name, expected, None)
            .await
    }

    /// Conditional non-revoking removal with a host-owned reconciliation
    /// guard. The guard is evaluated after the lifecycle lock is acquired and
    /// before comparing/removing state, closing the check-before-await window
    /// for a stale reload job.
    pub async fn remove_without_revoking_auth_if_config_guarded(
        &self,
        name: &str,
        expected: &McpServerConfig,
        guard: Arc<McpOperationGuard>,
    ) -> Result<bool, McpError> {
        self.remove_without_revoking_auth_if_config_with_guard(name, expected, Some(&*guard))
            .await
    }

    async fn remove_without_revoking_auth_if_config_with_guard(
        &self,
        name: &str,
        expected: &McpServerConfig,
        operation_guard: Option<&McpOperationGuard>,
    ) -> Result<bool, McpError> {
        let lifecycle = self.lifecycle_lock(name);
        let _guard = lifecycle.lock().await;
        if operation_guard.is_some_and(|guard| !guard()) {
            return Ok(false);
        }
        let matches = self
            .connections
            .read()
            .await
            .get(name)
            .is_some_and(|state| Self::same_config_snapshot(state.config(), expected));
        // Reading the snapshot can suspend behind a catalog writer after the
        // lifecycle guard passed. Recheck intent before starting teardown so
        // a reverted reload cannot delete the still-current configuration.
        if !matches || operation_guard.is_some_and(|guard| !guard()) {
            return Ok(false);
        }
        self.disconnect_locked_inner(name, false, true).await?;
        Ok(true)
    }

    /// Toggle one registered server immediately and retain the updated config
    /// for subsequent reconnects/startups. Disabling retires the live transport
    /// and dynamic tool partition without revoking OAuth credentials; enabling
    /// performs a fresh connect in the current session.
    ///
    /// Returns `Ok(None)` when the config was already in the requested state (a
    /// no-op — claude's `p` filter excludes it). Otherwise `Ok(Some(state))`
    /// carries the server's post-toggle action state (claude's fulfilled
    /// `u(name).type`): after disable it is [`lingxi_core::host::McpActionState::Disabled`];
    /// after enable it is the live post-connect state read back from the
    /// registry (`Connected` / `Failed` / `NeedsAuth` / …). Crucially, a failed
    /// enable **connect** is NOT surfaced as `Err` — the server flips on but
    /// stays disconnected, mirroring claude's `u(name)` fulfilling with
    /// `{type:"failed"}`; only a failed **disable teardown** stays an `Err`
    /// (claude's rejected promise → the server "couldn't be changed").
    pub async fn set_disabled(
        &self,
        name: &str,
        disabled: bool,
    ) -> Result<Option<lingxi_core::host::McpActionState>, McpError> {
        let lifecycle = self.lifecycle_lock(name);
        let _guard = lifecycle.lock().await;
        self.kick_pending_transport_cleanups().await;

        let (mut config, current_connection) = {
            let conns = self.connections.read().await;
            let Some(state) = conns.get(name) else {
                return Err(McpError::Internal(format!(
                    "no MCP server named \"{name}\""
                )));
            };
            if state.config().disabled == disabled {
                return Ok(None);
            }
            let current_connection = match state {
                McpConnectionState::Connected { connection_id, .. }
                | McpConnectionState::HealthChecking { connection_id, .. } => {
                    Some((*connection_id, true))
                }
                McpConnectionState::Cached { connection_id, .. } => Some((*connection_id, false)),
                _ => None,
            };
            (state.config().clone(), current_connection)
        };
        let invalidated_slot = self.invalidate_lazy_upgrade_slot(name).await;

        if disabled {
            // Keep the live state/client until the transport confirms teardown.
            // This makes a failed disable visible and safely retryable.
            if let Some((connection_id, true)) = current_connection {
                self.transport.disconnect(connection_id).await?;
            }
            config.disabled = true;
            let retired_config = config.clone();
            self.connections.write().await.insert(
                name.to_string(),
                McpConnectionState::Disconnected {
                    config,
                    last_error: None,
                },
            );
            self.clients.write().await.shift_remove(name);
            self.clear_prompt_predecessors_for_key(name).await;
            Self::finish_invalidated_lazy_upgrade_slot(invalidated_slot.as_ref());
            if let Some((connection_id, _)) = current_connection {
                self.emit_retire_event_if_shared(&retired_config, name, connection_id)
                    .await;
            }
            return Ok(Some(lingxi_core::host::McpActionState::Disabled));
        }

        config.disabled = false;
        self.connections.write().await.insert(
            name.to_string(),
            McpConnectionState::Disconnected {
                config: config.clone(),
                last_error: None,
            },
        );
        self.clear_prompt_predecessors_for_key(name).await;
        Self::finish_invalidated_lazy_upgrade_slot(invalidated_slot.as_ref());
        // A failed enable connect is a SETTLED "failed" outcome, not an error:
        // `connect_locked` already records `Disconnected { last_error }` on
        // failure, so the server flips on but reads back as `Failed`
        // ("not connected"). This mirrors claude's `u(name)` fulfilling with
        // `{type:"failed"}` rather than rejecting, so the /mcp handler can emit
        // "Enabled …, but it isn't connected yet." instead of a hard error.
        let _ = self.connect_locked(config, Some(name.to_string())).await;
        let resulting = {
            let conns = self.connections.read().await;
            conns.get(name).map_or(
                lingxi_core::host::McpActionState::Failed,
                project_action_state,
            )
        };
        Ok(Some(resulting))
    }

    /// Reconnect a single known server by name (`/mcp reconnect <server>`):
    /// tear down the live connection and re-establish it from the config the
    /// registry retains in every connection state. `Err(McpError::Internal)`
    /// when no server by that name is registered (callers pre-check via
    /// [`Self::server_names`] for the user-facing "no server named" message).
    pub async fn reconnect(&self, name: &str) -> Result<(), McpError> {
        let lifecycle = self.lifecycle_lock(name);
        let _guard = lifecycle.lock().await;
        // Every connection state carries its originating config; pull it out so
        // we can re-`connect` after tearing the live connection down.
        let config = {
            let conns = self.connections.read().await;
            let Some(state) = conns.get(name) else {
                return Err(McpError::Internal(format!(
                    "no MCP server named \"{name}\""
                )));
            };
            state.config().clone()
        };
        // Best-effort teardown (a never-connected server is a no-op), then a
        // fresh connect. `connect` early-returns the existing id if already
        // connected, so the disconnect must land first.
        self.disconnect_locked(name).await?;
        // Thread `name` back in as the table key: for an ordinary
        // (unscoped) server `name == config.name` already, so this is a
        // no-op; for an agent-scoped entry it keeps the reconnected
        // connection under the SAME scoped key it was torn down from,
        // instead of falling back to the plain `config.name` and stranding
        // the subagent's dispatch target.
        self.connect_locked(config, Some(name.to_string()))
            .await
            .map(|_| ())
    }

    /// Re-establish a stale Streamable HTTP session without revoking the
    /// server's OAuth grant. A 400/404 session-id failure invalidates only the
    /// transport session; treating it like a user-requested disconnect would
    /// log the user out and diverge from Claude Code's connection-cache reset.
    async fn reconnect_preserving_auth(&self, name: &str) -> Result<(), McpError> {
        let lifecycle = self.lifecycle_lock(name);
        let _guard = lifecycle.lock().await;
        let config = {
            let conns = self.connections.read().await;
            let Some(state) = conns.get(name) else {
                return Err(McpError::Internal(format!(
                    "no MCP server named \"{name}\""
                )));
            };
            state.config().clone()
        };
        self.disconnect_locked_inner(name, false, false).await?;
        self.connect_locked(config, Some(name.to_string()))
            .await
            .map(|_| ())
    }

    /// The names of every registered server (any connection state), sorted —
    /// the set `/mcp reconnect all` iterates.
    pub async fn server_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.connections.read().await.keys().cloned().collect();
        names.sort();
        names
    }

    /// Project every known connection into the trait-facing
    /// [`lingxi_core::host::McpServerInfo`] shape. Used by
    /// `OrchestratorHandle::list_mcp_servers` (M6-07) so `/mcp` can list
    /// the registry without exposing the internal state-machine enum.
    ///
    /// Returned list is sorted by `name` for stable display order.
    pub async fn snapshot(&self) -> Vec<lingxi_core::host::McpServerInfo> {
        let conns = self.connections.read().await;
        let mut out: Vec<lingxi_core::host::McpServerInfo> = conns
            .values()
            .map(|s| lingxi_core::host::McpServerInfo {
                name: s.name().to_string(),
                status: project_status(s),
                transport: s.transport_kind().to_string(),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Fine-grained `(name, McpActionState)` pairs for the `/mcp` action
    /// handler (`reconnect|enable|disable`). Unlike [`Self::snapshot`], this
    /// preserves the full state vocabulary (pending / disabled / needs-auth /
    /// failed) the handler needs to pick claude-code's byte-exact state-aware
    /// message. Sorted by name for stable display.
    pub async fn action_states(&self) -> Vec<(String, lingxi_core::host::McpActionState)> {
        let conns = self.connections.read().await;
        let mut out: Vec<(String, lingxi_core::host::McpActionState)> = conns
            .values()
            .map(|s| (s.name().to_string(), project_action_state(s)))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Failed servers with their sanitized error text, for the `ToolSearch`
    /// empty-result diagnostics note — a port of claude-code's `wZr(u())`
    /// (`failed_mcp_servers`). Every server whose action state projects to
    /// [`lingxi_core::host::McpActionState::Failed`] ("not connected") is included,
    /// carrying its recorded error where one exists, sanitized through
    /// [`sanitize_diagnostic`] (claude's `xLt`). Both the name and the error are
    /// sanitized (claude sanitizes both); the error is the untrusted, model-
    /// visible part. Sorted by name.
    ///
    /// Unlike claude, the port carries no per-server `errorCode`, so it cannot
    /// distinguish the `UNCONFIGURED` state claude's `kee` excludes — every
    /// projected-`Failed` server is surfaced.
    pub async fn failed_action_servers(&self) -> Vec<(String, Option<String>)> {
        let conns = self.connections.read().await;
        let mut out: Vec<(String, Option<String>)> = conns
            .values()
            .filter(|s| project_action_state(s) == lingxi_core::host::McpActionState::Failed)
            .map(|s| {
                let error = match s {
                    McpConnectionState::Failed { error, .. } => Some(error.clone()),
                    McpConnectionState::Disconnected { last_error, .. } => last_error.clone(),
                    _ => None,
                };
                (
                    sanitize_diagnostic(s.name()),
                    error.map(|e| sanitize_diagnostic(&e)),
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Names of servers currently in a PENDING (still-connecting) state —
    /// `Connecting` / `AwaitingOAuth` / `Reconnecting` (claude-code's MCP client
    /// `type === "pending"`). These may yet expose tools, so the `AgentTool`
    /// required-MCP gate waits on them before failing. NOTE: the public
    /// [`lingxi_core::host::McpStatus`] UI projection collapses these into `Disconnected`;
    /// this reads the INTERNAL state map so a connecting server is
    /// distinguishable from a failed/absent one (the gap that blocked the
    /// 30s poll-wait).
    pub async fn servers_pending(&self) -> Vec<String> {
        self.connections
            .read()
            .await
            .values()
            .filter(|s| {
                matches!(
                    s,
                    McpConnectionState::Connecting { .. }
                        | McpConnectionState::AwaitingOAuth { .. }
                        | McpConnectionState::Reconnecting { .. }
                )
            })
            .map(|s| s.name().to_string())
            .collect()
    }

    /// Recompute and store the [`Self::has_pending_servers`] mirror, returning
    /// the fresh value.
    ///
    /// This is claude-code's `eZf()` (`bdl(b7e()??[]).length>0`,
    /// `cc-238.js @229641619`), where `bdl` filters MCP clients on
    /// `type === "pending"`. [`project_action_state`] reproduces that
    /// discriminant, so `needs-auth` (a SEPARATE client type upstream) does NOT
    /// count as pending here — unlike [`Self::servers_pending`], which
    /// deliberately folds `AwaitingOAuth` in for the `AgentTool` required-MCP
    /// poll-wait.
    ///
    /// Call it from an async seam right before assembling a tool list; the
    /// `WaitForMcpServers` tool's synchronous `is_enabled` then reads the
    /// mirror.
    pub async fn refresh_pending_servers(&self) -> bool {
        let pending = self
            .connections
            .read()
            .await
            .values()
            .any(|s| project_action_state(s) == lingxi_core::host::McpActionState::Pending);
        self.pending_servers
            .store(pending, std::sync::atomic::Ordering::Relaxed);
        pending
    }

    /// Synchronous read of the pending-server mirror last written by
    /// [`Self::refresh_pending_servers`].
    #[must_use]
    pub fn has_pending_servers(&self) -> bool {
        self.pending_servers
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Names of servers in a terminal FAILED state (`Failed` — exhausted retries;
    /// claude-code's MCP client `type === "failed"`). The required-MCP gate's
    /// poll-wait stops early when a required server fails rather than waiting out
    /// the full deadline.
    pub async fn servers_failed(&self) -> Vec<String> {
        self.connections
            .read()
            .await
            .values()
            .filter(|s| matches!(s, McpConnectionState::Failed { .. }))
            .map(|s| s.name().to_string())
            .collect()
    }

    /// The set of MCP server names that currently expose at least one tool —
    /// i.e. servers that are connected AND authenticated (an unauthenticated
    /// server has no tools). Port of claude-code's `serversWithTools` derivation
    /// in `AgentTool.call` (`AgentTool.tsx:394-405`): claude scans
    /// `appState.mcp.tools` for `mcp__<server>__<tool>` and collects the distinct
    /// `<server>` part. Here we ask each registered [`McpClient`] for its tools
    /// and extract the server segment from each tool's `full_name`
    /// (`mcp__<server>__<tool>`), so a server with zero tools (e.g. still
    /// awaiting OAuth) is correctly absent.
    ///
    /// Used by `AgentTool`'s pre-spawn `required_mcp_servers` gate
    /// (`AgentTool.tsx:367-409`). Returned list is in DISCOVERY (insertion)
    /// order and deduplicated — claude builds `serversWithTools` by iterating
    /// `appState.mcp.tools` in order and pushing first-seen server names, with
    /// NO sort, so the required-MCP error lists servers in that same order.
    pub async fn servers_with_tools(&self) -> Vec<String> {
        let clients: Vec<(String, Arc<McpClient>)> = self
            .clients
            .read()
            .await
            .iter()
            .map(|(name, entry)| (name.clone(), Arc::clone(&entry.client)))
            .collect();
        // §11 Stage 2: `cache_only` names a `Cached` server that has NO
        // registered client yet (`clients` is a separate map — see
        // `McpConnectionState::Cached`'s doc) — it must still contribute its
        // tools here, or `AgentTool`'s required-MCP gate would wrongly refuse
        // a subagent spawn naming a server the model's own tool list already
        // shows as available. `cached` (both `Connected` and `Cached`) is
        // reused as the fast-path source for a server that DOES have a
        // client, same as before this change.
        let (cached, cache_only): (
            HashMap<String, Vec<lingxi_core::host::McpToolDto>>,
            Vec<String>,
        ) = {
            let conns = self.connections.read().await;
            let mut cached = HashMap::new();
            let mut cache_only = Vec::new();
            for (name, state) in conns.iter() {
                match state {
                    McpConnectionState::Connected { tools, .. } => {
                        cached.insert(name.clone(), tools.clone());
                    }
                    McpConnectionState::Cached { tools, .. } => {
                        cached.insert(name.clone(), tools.clone());
                        cache_only.push(name.clone());
                    }
                    _ => {}
                }
            }
            (cached, cache_only)
        };
        let mut out: Vec<String> = Vec::new();
        let push_tools = |tools: Vec<lingxi_core::host::McpToolDto>, out: &mut Vec<String>| {
            for tool in tools {
                // `full_name` is `mcp__<server>__<tool>` (rewrite site in
                // `connect`); the server segment is index 1.
                let parts: Vec<&str> = tool.full_name.split("__").collect();
                if let Some(server) = parts.get(1) {
                    if !server.is_empty() && !out.iter().any(|s| s == server) {
                        out.push((*server).to_string());
                    }
                }
            }
        };
        for (name, client) in clients {
            let tools = if let Some(tools) = cached.get(&name) {
                tools.clone()
            } else {
                match client.list_tools().await {
                    Ok(tools) => tools,
                    Err(_) => continue,
                }
            };
            push_tools(tools, &mut out);
        }
        for name in cache_only {
            if let Some(tools) = cached.get(&name) {
                push_tools(tools.clone(), &mut out);
            }
        }
        out
    }

    /// Every prompt advertised by a CONNECTED server, as
    /// `(server_name, connection_id, prompt)`.
    ///
    /// Claude-code merges these into the slash-command list (`getAllCommands`
    /// folding in `mcp.commands`), which is what makes an MCP prompt reachable
    /// as `/<server>:<prompt>` and findable by the `Skill` tool. The registry
    /// has always FETCHED them (`prompts/list` during `connect`, refreshed on
    /// `notifications/prompts/list_changed`); nothing read them back.
    ///
    /// `Connected` and discovery-cache-served `Cached` servers contribute; a
    /// reconnecting or failed server's last-known prompts would advertise
    /// commands that cannot be fetched. Ordered by server name so the merged
    /// command list is deterministic.
    pub async fn connected_prompts(
        &self,
    ) -> Vec<(
        String,
        lingxi_core::types::McpConnectionId,
        lingxi_core::host::McpPromptDto,
    )> {
        let conns = self.connections.read().await;
        let mut servers: Vec<&String> = conns.keys().collect();
        servers.sort();
        let mut out = Vec::new();
        for name in servers {
            if let Some(
                crate::connection::McpConnectionState::Connected {
                    connection_id,
                    prompts,
                    ..
                }
                | crate::connection::McpConnectionState::Cached {
                    connection_id,
                    prompts,
                    ..
                },
            ) = conns.get(name)
            {
                for prompt in prompts {
                    out.push((name.clone(), *connection_id, prompt.clone()));
                }
            }
        }
        out
    }

    /// Render one prompt through the exact live connection that advertised it.
    ///
    /// Commands retain a [`McpConnectionId`] rather than only the logical server
    /// name so reconnecting a server cannot accidentally dispatch a stale menu
    /// entry through a newer connection generation.
    pub async fn get_prompt(
        &self,
        connection_id: lingxi_core::types::McpConnectionId,
        prompt_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let direct = {
            let connections = self.connections.read().await;
            connections.iter().find_map(|(name, state)| match state {
                crate::connection::McpConnectionState::Connected {
                    connection_id: current,
                    ..
                } if *current == connection_id => {
                    Some((name.clone(), None, None::<PromptPredecessor>))
                }
                crate::connection::McpConnectionState::Cached {
                    connection_id: current,
                    ..
                } if *current == connection_id => {
                    Some((name.clone(), Some(*current), None::<PromptPredecessor>))
                }
                _ => None,
            })
        };
        let (server_name, expected_live_connection_id, predecessor) =
            if let Some((server_name, cached_connection_id, predecessor)) = direct {
                let expected_live_connection_id = match cached_connection_id {
                    Some(_) => self.ensure_dialed_from_cache(&server_name).await?,
                    None => connection_id,
                };
                (server_name, expected_live_connection_id, predecessor)
            } else {
                let predecessor = self
                    .prompt_predecessors
                    .read()
                    .await
                    .get(&connection_id)
                    .cloned()
                    .ok_or_else(|| {
                        McpError::Internal(format!(
                            "MCP prompt connection {connection_id} is no longer active"
                        ))
                    })?;
                (
                    predecessor.key.clone(),
                    predecessor.live_connection_id,
                    Some(predecessor),
                )
            };

        {
            let conns = self.connections.read().await;
            match conns.get(&server_name) {
                Some(crate::connection::McpConnectionState::Connected {
                    connection_id: current,
                    config,
                    ..
                }) => {
                    if *current != expected_live_connection_id {
                        return Err(McpError::Internal(format!(
                            "MCP prompt connection {connection_id} is no longer active"
                        )));
                    }
                    if let Some(predecessor) = predecessor.as_ref() {
                        if !Self::same_config_snapshot(config, &predecessor.config) {
                            return Err(McpError::Internal(format!(
                                "MCP prompt connection {connection_id} is no longer active"
                            )));
                        }
                    }
                }
                _ => {
                    return Err(McpError::Internal(format!(
                        "MCP prompt connection {connection_id} is no longer active"
                    )))
                }
            }
        }

        let client = {
            let clients = self.clients.read().await;
            let entry = clients.get(&server_name).ok_or_else(|| {
                McpError::Internal(format!(
                    "MCP prompt server {server_name} has no live client"
                ))
            })?;
            if entry.connection_id != Some(expected_live_connection_id) {
                return Err(McpError::Internal(format!(
                    "MCP prompt connection {connection_id} is no longer active"
                )));
            }
            Arc::clone(&entry.client)
        };

        client
            .get_prompt(prompt_name, arguments)
            .await
            .map_err(|error| McpError::Internal(error.to_string()))
    }

    /// Recover the RAW wire tool name for a model-facing MCP tool `full_name`.
    ///
    /// The model-facing `full_name` (`mcp__<normalize(server)>__<normalize(tool)>`,
    /// the rewrite site in [`Self::connect`]) carries the NORMALIZED tool
    /// segment, 1:1 with claude-code's `buildMcpToolName`. The MCP server,
    /// however, expects the UNNORMALIZED wire name in its `tools/call` request.
    /// claude-code keeps it as `mcpInfo.toolName` (`client.ts:1774`); here it
    /// lives on the cached [`lingxi_core::host::McpToolDto::tool_name`], so the dispatch
    /// path recovers it by matching the dto whose `full_name` equals the
    /// model-supplied name.
    ///
    /// `normalized_server` is the FQN's server segment (already normalized).
    /// Returns `None` when no connected server matches it or `full_name` is
    /// unknown — the caller then falls back to the parsed (normalized) segment,
    /// a no-op for valid-identifier names where raw == normalized.
    ///
    /// `table_key`, when `Some`, restricts the search to the ONE entry
    /// stored under that exact table key (§24b agent-scoped dispatch) —
    /// without it, two connections sharing the same plain `config.name` (an
    /// agent-scoped inline server and a same-named shared/session server)
    /// would resolve ambiguously against whichever one `HashMap` iteration
    /// happens to visit first. `None` preserves the original behaviour
    /// exactly: scan every connection by normalized display name.
    pub async fn resolve_wire_tool_name(
        &self,
        normalized_server: &str,
        full_name: &str,
        table_key: Option<&str>,
    ) -> Option<String> {
        let conns = self.connections.read().await;
        for (key, state) in conns.iter() {
            if let Some(want) = table_key {
                if key != want {
                    continue;
                }
            }
            // §11 Stage 2: a `Cached` server's tool dtos are the SAME
            // catalog a live `Connected` one would carry (served from disk
            // instead of the wire), so the raw-name recovery is identical.
            if let McpConnectionState::Connected { config, tools, .. }
            | McpConnectionState::Cached { config, tools, .. } = state
            {
                if normalize_name_for_mcp(&config.name) == normalized_server {
                    return tools
                        .iter()
                        .find(|dto| dto.full_name == full_name)
                        .map(|dto| dto.tool_name.clone());
                }
            }
        }
        None
    }
}

/// §24b: build the internal `connections`/`clients` table key for a
/// per-SUBAGENT inline `mcpServers` entry. Uses ONLY
/// `[a-zA-Z0-9_-]` (normalizing `server_name` and rendering `agent_id`'s bare
/// UUID, never its `agent:`-prefixed [`std::fmt::Display`]) so
/// `normalize_name_for_mcp` is the IDENTITY on the result — the fuzzy
/// by-normalized-name lookups in [`McpRegistry::get_client`] /
/// [`McpRegistry::get_config`] / [`McpRegistry::has_callable_server`] /
/// [`McpRegistry::call_tool_with_auth_retry`] therefore match this key by
/// plain string equality when a caller passes it verbatim, exactly as they
/// match a normal (unscoped) `config.name`. Two different agent spawns produce
/// two different keys even for the identical plain server name, since each
/// carries its own [`AgentId`].
#[must_use]
pub fn agent_scope_table_key(agent_id: AgentId, server_name: &str) -> String {
    format!(
        "__lingxi_agent_scope__{}__{}",
        agent_id.as_uuid(),
        normalize_name_for_mcp(server_name)
    )
}

/// MCPLIFE.4: the connect+initialize handshake deadline, mirroring claude-code's
/// `getConnectionTimeoutMs()` (`services/mcp/client.ts:456-458`):
/// `parseInt(process.env.MCP_TIMEOUT || '', 10) || 30000` — a positive integer
/// number of milliseconds, defaulting to 30s when unset / non-numeric / zero.
pub(crate) fn mcp_connection_timeout() -> Duration {
    let ms = std::env::var("MCP_TIMEOUT")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(30_000);
    Duration::from_millis(ms)
}

fn cleanup_disconnect_timeout() -> Duration {
    Duration::from_secs(5)
}

/// Exponential backoff for reconnect `attempt` (1-based), capped at
/// [`MAX_BACKOFF`]. Mirrors claude-code's
/// `min(INITIAL_BACKOFF_MS * 2^(attempt-1), MAX_BACKOFF_MS)`.
fn backoff_for(attempt: u32) -> Duration {
    let factor = 1u64
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u64::MAX);
    let base = u64::try_from(INITIAL_BACKOFF.as_millis()).unwrap_or(u64::MAX);
    let millis = base.saturating_mul(factor);
    Duration::from_millis(millis).min(MAX_BACKOFF)
}

/// Backoff to sleep AFTER reconnect `attempt` (1-based) within a run of `max`
/// attempts, or `None` when no sleep should occur.
///
/// Encodes claude-code's schedule (`useManageMCPConnections.ts:446-460`): a
/// backoff is taken only after a failed NON-final attempt; the final attempt
/// (`attempt == max`) has no trailing sleep, and — because attempt 1 fires
/// immediately — there is never a leading sleep. For the default `max == 5`
/// the sleeps are 1s, 2s, 4s, 8s, so attempts fire at t = 0, 1, 3, 7, 15s; the
/// 16s / 30s-cap value is therefore never used.
fn post_attempt_backoff(attempt: u32, max: u32) -> Option<Duration> {
    if attempt >= max {
        None
    } else {
        Some(backoff_for(attempt))
    }
}

#[cfg(test)]
#[path = "registry/tests/connection_timeout_tests.rs"]
mod connection_timeout_tests;

#[cfg(test)]
#[path = "registry/tests/backoff_schedule_tests.rs"]
mod backoff_schedule_tests;

/// Test-only re-exports of internal OAuth-error classifiers, so integration
/// tests (`tests/oauth_flow_test.rs`) can unit-check the step-up detection
/// without making the helpers part of the public API.
#[doc(hidden)]
pub mod test_support {
    use lingxi_core::host::McpError;

    /// See [`super::error_is_403_insufficient_scope`].
    #[must_use]
    pub fn error_is_403_insufficient_scope(e: &McpError) -> Option<String> {
        super::error_is_403_insufficient_scope(e)
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
struct CapturedMcpTelemetryEvent {
    name: &'static str,
    payload: serde_json::Value,
}

#[cfg(test)]
fn test_telemetry_events() -> &'static StdMutex<Vec<CapturedMcpTelemetryEvent>> {
    static EVENTS: OnceLock<StdMutex<Vec<CapturedMcpTelemetryEvent>>> = OnceLock::new();
    EVENTS.get_or_init(|| StdMutex::new(Vec::new()))
}

#[cfg(test)]
fn test_telemetry_capture_lock() -> &'static StdMutex<()> {
    static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| StdMutex::new(()))
}

#[cfg(test)]
fn clear_test_telemetry_events() {
    test_telemetry_events().lock().unwrap().clear();
}

#[cfg(test)]
fn take_test_telemetry_events() -> Vec<CapturedMcpTelemetryEvent> {
    std::mem::take(&mut *test_telemetry_events().lock().unwrap())
}

#[cfg(test)]
fn record_test_telemetry_event(name: &'static str, payload: serde_json::Value) {
    test_telemetry_events()
        .lock()
        .unwrap()
        .push(CapturedMcpTelemetryEvent { name, payload });
}

#[cfg(test)]
fn catalog_change_listener_pause_slot() -> &'static StdMutex<Option<Arc<Notify>>> {
    static SLOT: OnceLock<StdMutex<Option<Arc<Notify>>>> = OnceLock::new();
    SLOT.get_or_init(|| StdMutex::new(None))
}

#[cfg(test)]
fn set_catalog_change_listener_pause_for_test(hook: Option<Arc<Notify>>) {
    *catalog_change_listener_pause_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook;
}

#[cfg(test)]
fn catalog_change_listener_closed_slot() -> &'static StdMutex<Option<Arc<Notify>>> {
    static SLOT: OnceLock<StdMutex<Option<Arc<Notify>>>> = OnceLock::new();
    SLOT.get_or_init(|| StdMutex::new(None))
}

#[cfg(test)]
fn set_catalog_change_listener_closed_for_test(hook: Option<Arc<Notify>>) {
    *catalog_change_listener_closed_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook;
}

#[cfg(test)]
fn listener_reopen_park_jitter_slot() -> &'static StdMutex<Option<f64>> {
    static SLOT: OnceLock<StdMutex<Option<f64>>> = OnceLock::new();
    SLOT.get_or_init(|| StdMutex::new(None))
}

#[cfg(test)]
fn set_listener_reopen_park_jitter_for_test(jitter: Option<f64>) {
    *listener_reopen_park_jitter_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = jitter;
}

#[cfg(test)]
fn test_listener_reopen_park_jitter() -> Option<f64> {
    *listener_reopen_park_jitter_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
async fn maybe_pause_catalog_change_listener_for_test() {
    let hook = catalog_change_listener_pause_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook.notified().await;
    }
}

#[cfg(test)]
fn notify_catalog_change_listener_closed_for_test() {
    let hook = catalog_change_listener_closed_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    if let Some(hook) = hook {
        hook.notify_waiters();
    }
}

/// Whether a state's config is flagged `disabled` (mid-reconnect guard).
fn state_is_disabled(state: &McpConnectionState) -> bool {
    state.config().disabled
}

/// Project a [`McpConnectionState`] onto the fine-grained
/// [`lingxi_core::host::McpActionState`] used by the `/mcp reconnect|enable|disable`
/// action handler — a faithful mirror of claude-code's client `type`
/// discriminant. The `config.disabled` gate takes precedence (a disabled
/// server reports `"disabled"` regardless of its last live state), then:
/// `Connected`/`HealthChecking` → connected, `Connecting`/`Reconnecting` →
/// pending, `AwaitingOAuth` → needs-auth, everything else (`Failed`,
/// `Disconnected`, `Stopped`) → failed ("not connected").
fn project_action_state(state: &McpConnectionState) -> lingxi_core::host::McpActionState {
    use lingxi_core::host::McpActionState;
    if state_is_disabled(state) {
        return McpActionState::Disabled;
    }
    match state {
        // §11 Stage 2 — a `Cached` server presents identically to a live
        // `Connected` one for `/mcp` action reporting: the whole point of
        // serving from cache is that the user perceives no difference.
        McpConnectionState::Connected { .. }
        | McpConnectionState::Cached { .. }
        | McpConnectionState::HealthChecking { .. } => McpActionState::Connected,
        McpConnectionState::Connecting { .. } | McpConnectionState::Reconnecting { .. } => {
            McpActionState::Pending
        }
        McpConnectionState::AwaitingOAuth { .. } => McpActionState::NeedsAuth,
        McpConnectionState::Failed { .. }
        | McpConnectionState::Disconnected { .. }
        | McpConnectionState::Stopped { .. } => McpActionState::Failed,
    }
}

/// Project a [`McpConnectionState`] variant onto the trait-facing
/// [`lingxi_core::host::McpStatus`] (M6-07).
fn project_status(state: &McpConnectionState) -> lingxi_core::host::McpStatus {
    use lingxi_core::host::McpStatus;
    match state {
        // §11 Stage 2 — same rationale as `project_action_state`: a cached
        // server reports `Connected`, never a distinct status.
        McpConnectionState::Connected { .. } | McpConnectionState::Cached { .. } => {
            McpStatus::Connected
        }
        McpConnectionState::Disconnected {
            last_error: Some(e),
            ..
        } => McpStatus::Error(e.clone()),
        McpConnectionState::Disconnected { .. }
        | McpConnectionState::Connecting { .. }
        | McpConnectionState::AwaitingOAuth { .. }
        | McpConnectionState::HealthChecking { .. }
        | McpConnectionState::Reconnecting { .. }
        | McpConnectionState::Stopped { .. } => McpStatus::Disconnected,
        McpConnectionState::Failed { error, .. } => McpStatus::Error(error.clone()),
    }
}

#[cfg(test)]
#[path = "registry/tests/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "registry/tests/snapshot_tests.rs"]
mod snapshot_tests;
