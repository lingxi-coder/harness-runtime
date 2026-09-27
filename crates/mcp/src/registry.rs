//! Owns every MCP connection and drives the state machine.
//!
//! Engine code holds an `Arc<McpRegistry>` and uses [`Self::connect`] /
//! [`Self::disconnect`] to manage servers. Startup auto-connect
//! ([`McpRegistry::connect_all`]) and the reconnect/backoff loop
//! ([`McpRegistry::run_reconnect_loop`]) implement the "Plan 13" wiring.

use crate::client::McpClient;
use crate::connection::{ConfigScope, McpConnectionState, McpServerConfig};
use crate::hook_dispatch::HookDispatcher;
use crate::normalization::normalize_name_for_mcp;
use crate::oauth::{self, OnAuthorizationUrl};
use crate::raw_conn::RawConnectionProvider;
use futures_util::FutureExt as _;
use indexmap::IndexMap;
use platform_api::{
    Clock, HttpTransport, McpError, McpNotificationStream, McpRawConnection, McpTransport,
    McpTransportSpec, SecureStorage, ServerCapabilitiesDto,
};
use protocol::{AgentId, McpConnectionId};
use rand::Rng as _;
use std::collections::HashMap;
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime};
use tokio::sync::{broadcast, Mutex, Notify, RwLock};

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
    negotiated: platform_api::McpNegotiatedProtocol,
    capabilities: ServerCapabilitiesDto,
    tools: Vec<platform_api::McpToolDto>,
    resources: Vec<platform_api::McpResourceDto>,
    resource_templates: Vec<platform_api::McpResourceTemplateDto>,
    prompts: Vec<platform_api::McpPromptDto>,
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
) -> platform_api::McpNegotiatedProtocol {
    let era = match entry.negotiated_era.as_deref() {
        Some("modern") => platform_api::McpProtocolEra::Modern,
        _ => platform_api::McpProtocolEra::Legacy,
    };
    platform_api::McpNegotiatedProtocol {
        era,
        version: match era {
            platform_api::McpProtocolEra::Modern => "2026-07-28",
            platform_api::McpProtocolEra::Legacy => "2025-11-25",
        }
        .to_string(),
    }
}

fn negotiated_era_label(era: platform_api::McpProtocolEra) -> &'static str {
    match era {
        platform_api::McpProtocolEra::Modern => "modern",
        platform_api::McpProtocolEra::Legacy => "legacy",
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
    async fn managed_local_app_connection_id(&self, server_name: &str) -> McpConnectionId {
        let connections = self.connections.read().await;
        match connections.get(server_name) {
            Some(McpConnectionState::Connected { connection_id, .. })
            | Some(McpConnectionState::Cached { connection_id, .. })
            | Some(McpConnectionState::HealthChecking { connection_id, .. }) => *connection_id,
            _ => McpConnectionId::new(),
        }
    }

    fn managed_local_app_runtime_changed(
        previous: &ManagedLocalAppRuntime,
        current: &ManagedLocalAppRuntime,
    ) -> (bool, bool) {
        (
            previous.enabled != current.enabled || previous.enabled_tools != current.enabled_tools,
            previous.enabled != current.enabled || previous.resource != current.resource,
        )
    }

    async fn remove_local_app_exposures(&self, app_id: &str) -> bool {
        let mut conversations = self.local_app_exposures.write().await;
        let mut removed = false;
        for state in conversations.values_mut() {
            if state.entries.remove(app_id).is_some() {
                state.next_generation = state.next_generation.saturating_add(1);
                removed = true;
            }
        }
        removed
    }

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

    /// Register or refresh one published Local App logical server. Only a
    /// changed tool surface advances the logical generation and emits the
    /// shared tools/list_changed notification; build/execution-only changes
    /// update the catalog pointer without invalidating connections.
    pub async fn register_managed_local_app(
        &self,
        scope: ConversationExport,
        catalog_sha256: String,
        _surface_changed: bool,
    ) -> Result<ManagedLocalAppServer, McpError> {
        if !is_sha256(&catalog_sha256) {
            return Err(McpError::Internal(
                "invalid Local App catalog identity".into(),
            ));
        }
        let mut apps = self.managed_local_apps.write().await;
        // The catalog commit is the authority for whether the exposed tool
        // surface changed. Do not trust a caller-supplied boolean: a stale or
        // forged hint must not produce duplicate listChanged notifications,
        // nor suppress one when a new surface is actually committed.
        let actual_surface_changed = apps.get(&scope.app_id).is_none_or(|server| {
            server.scope.listed_tool_surface_sha256 != scope.listed_tool_surface_sha256
        });
        let previous = apps.get(&scope.app_id).cloned();
        let actual_catalog_changed = previous
            .as_ref()
            .is_some_and(|server| server.catalog_sha256 != catalog_sha256);
        let generation = previous
            .as_ref()
            .map(|server| server.surface_generation + u64::from(actual_surface_changed))
            .unwrap_or(1);
        let server = ManagedLocalAppServer {
            scope: scope.clone(),
            catalog_sha256,
            surface_generation: generation,
        };
        apps.insert(scope.app_id.clone(), server.clone());
        drop(apps);
        self.managed_local_app_runtime
            .write()
            .await
            .entry(scope.app_id.clone())
            .or_insert_with(ManagedLocalAppRuntime::default);
        if actual_surface_changed {
            let connection_id = self
                .managed_local_app_connection_id(&scope.server_name())
                .await;
            let _ = self.catalog_changes.send(McpCatalogChanged {
                server_name: scope.server_name(),
                connection_id,
                retired_connection_id: None,
                kind: McpCatalogKind::Tools,
                telemetry_cause: None,
            });
        }
        if previous.is_some() && actual_catalog_changed {
            let connection_id = self
                .managed_local_app_connection_id(&scope.server_name())
                .await;
            let _ = self.catalog_changes.send(McpCatalogChanged {
                server_name: scope.server_name(),
                connection_id,
                retired_connection_id: None,
                kind: McpCatalogKind::Resources,
                telemetry_cause: None,
            });
        }
        Ok(server)
    }

    /// Remove a published Local App logical server after Host has stopped new
    /// calls. The notification tells consumers to evict its exposed tools.
    pub async fn unregister_managed_local_app(&self, app_id: &str) -> Result<bool, McpError> {
        if !is_local_app_id(app_id) {
            return Err(McpError::Internal("invalid Local App identity".into()));
        }
        let removed = self.managed_local_apps.write().await.remove(app_id);
        if removed.is_some() {
            self.managed_local_app_runtime.write().await.remove(app_id);
            // A deleted app can no longer be selected or called. Remove its
            // logical exposure from every conversation in the same commit
            // boundary; no stale FQN survives deletion.
            self.remove_local_app_exposures(app_id).await;
            let _ = self.catalog_changes.send(McpCatalogChanged {
                server_name: format!("local_app_{app_id}"),
                connection_id: McpConnectionId::new(),
                retired_connection_id: None,
                kind: McpCatalogKind::Tools,
                telemetry_cause: None,
            });
            let _ = self.catalog_changes.send(McpCatalogChanged {
                server_name: format!("local_app_{app_id}"),
                connection_id: McpConnectionId::new(),
                retired_connection_id: None,
                kind: McpCatalogKind::Resources,
                telemetry_cause: None,
            });
        }
        Ok(removed.is_some())
    }

    /// Lightweight logical-server count; all entries continue to use this
    /// registry's one physical transport substrate.
    pub async fn managed_local_app_count(&self) -> usize {
        self.managed_local_apps.read().await.len()
    }

    /// Snapshot every published Local App logical server.
    pub async fn managed_local_apps(&self) -> Vec<ManagedLocalAppServer> {
        let mut apps: Vec<ManagedLocalAppServer> = self
            .managed_local_apps
            .read()
            .await
            .values()
            .cloned()
            .collect();
        apps.sort_by(|left, right| left.scope.app_id.cmp(&right.scope.app_id));
        apps
    }

    /// There is exactly one physical transport owned by this registry.
    #[must_use]
    pub fn physical_transport_count(&self) -> usize {
        1
    }

    pub async fn managed_local_app(&self, app_id: &str) -> Option<ManagedLocalAppServer> {
        self.managed_local_apps.read().await.get(app_id).cloned()
    }

    /// Snapshot the Host-owned runtime overlay for one managed Local App.
    pub async fn managed_local_app_runtime(&self, app_id: &str) -> Option<ManagedLocalAppRuntime> {
        self.managed_local_app_runtime
            .read()
            .await
            .get(app_id)
            .cloned()
    }

    /// Update the Host-owned runtime overlay for one managed Local App.
    ///
    /// This is the intended seam for service enable/disable, per-tool
    /// allowlists, and per-app widget resource publication without changing
    /// the immutable published catalog record.
    pub async fn set_managed_local_app_runtime(
        &self,
        app_id: &str,
        enabled: bool,
        enabled_tools: Option<Vec<String>>,
        resource: Option<ManagedLocalAppResource>,
    ) -> Result<ManagedLocalAppRuntime, McpError> {
        if !is_local_app_id(app_id) {
            return Err(McpError::Internal("invalid Local App identity".into()));
        }
        let Some(server) = self.managed_local_app(app_id).await else {
            return Err(McpError::ToolNotFound(app_id.into()));
        };
        let enabled_tools = enabled_tools
            .map(|tools| {
                let mut normalized = Vec::with_capacity(tools.len());
                for tool in tools {
                    if !is_local_app_tool_name(&tool) {
                        return Err(McpError::ToolNotFound(tool));
                    }
                    let _ = server.scope.tool_full_name(&tool)?;
                    if !normalized.iter().any(|existing| existing == &tool) {
                        normalized.push(tool);
                    }
                }
                normalized.sort();
                Ok(normalized)
            })
            .transpose()?;
        if let Some(resource) = resource.as_ref() {
            if resource.uri.trim().is_empty() || resource.name.trim().is_empty() {
                return Err(McpError::Internal(
                    "managed Local App resource metadata is incomplete".into(),
                ));
            }
        }
        let mut runtimes = self.managed_local_app_runtime.write().await;
        let previous = runtimes.get(app_id).cloned().unwrap_or_default();
        let resource_generation_changed =
            previous.resource != resource || previous.enabled != enabled;
        let runtime = ManagedLocalAppRuntime {
            enabled,
            enabled_tools,
            resource,
            resource_generation: previous.resource_generation
                + u64::from(resource_generation_changed),
        };
        let (tools_changed, resources_changed) =
            Self::managed_local_app_runtime_changed(&previous, &runtime);
        runtimes.insert(app_id.to_string(), runtime.clone());
        drop(runtimes);
        if !enabled {
            self.remove_local_app_exposures(app_id).await;
        }
        let connection_id = self
            .managed_local_app_connection_id(&server.scope.server_name())
            .await;
        if tools_changed {
            let _ = self.catalog_changes.send(McpCatalogChanged {
                server_name: server.scope.server_name(),
                connection_id,
                retired_connection_id: None,
                kind: McpCatalogKind::Tools,
                telemetry_cause: None,
            });
        }
        if resources_changed {
            let _ = self.catalog_changes.send(McpCatalogChanged {
                server_name: server.scope.server_name(),
                connection_id,
                retired_connection_id: None,
                kind: McpCatalogKind::Resources,
                telemetry_cause: None,
            });
        }
        Ok(runtime)
    }

    /// Expose a published Local App in one conversation. Exposure is lazy and
    /// bounded: at most eight logical apps are retained, with unpinned,
    /// idle least-recently-used entries evicted first. A pinned entry is the
    /// only hard pin; merely listing or calling an app keeps it recent but
    /// does not make it ineligible for eviction.
    pub async fn expose_managed_local_app_with_diff(
        &self,
        conversation_id: &str,
        app_id: &str,
        pin: bool,
    ) -> Result<LocalAppExposureUpdate, McpError> {
        if conversation_id.is_empty() || !is_local_app_id(app_id) {
            return Err(McpError::Internal(
                "invalid Local App exposure scope".into(),
            ));
        }
        if self.managed_local_app(app_id).await.is_none() {
            return Err(McpError::ToolNotFound(app_id.into()));
        }
        if self
            .managed_local_app_runtime(app_id)
            .await
            .is_some_and(|runtime| !runtime.enabled)
        {
            return Err(McpError::ToolNotFound(app_id.into()));
        }

        let mut conversations = self.local_app_exposures.write().await;
        let state = conversations
            .entry(conversation_id.to_string())
            .or_default();
        state.next_sequence = state.next_sequence.saturating_add(1);
        if let Some(entry) = state.entries.get_mut(app_id) {
            entry.last_used = state.next_sequence;
            if pin && !entry.pinned {
                state.next_generation = state.next_generation.saturating_add(1);
                entry.pinned = true;
                entry.exposure_generation = state.next_generation;
            }
            return Ok(LocalAppExposureUpdate {
                exposure: entry.clone(),
                evicted_app_id: None,
            });
        }

        let mut evicted_app_id = None;
        if state.entries.len() >= LOCAL_APP_MAX_EXPOSED {
            let evict = state
                .entries
                .iter()
                .filter(|(_, entry)| !entry.pinned && entry.in_flight == 0)
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(id, _)| id.clone());
            let Some(evict) = evict else {
                let mut pinned: Vec<&str> = state
                    .entries
                    .values()
                    .filter(|entry| entry.pinned)
                    .map(|entry| entry.app_id.as_str())
                    .collect();
                pinned.sort_unstable();
                return Err(McpError::Internal(format!(
                    "exposure_capacity_reached: pinned apps [{}]",
                    pinned.join(",")
                )));
            };
            state.entries.remove(&evict);
            evicted_app_id = Some(evict);
        }

        state.next_generation = state.next_generation.saturating_add(1);
        let entry = LocalAppExposure {
            app_id: app_id.to_string(),
            pinned: pin,
            in_flight: 0,
            last_used: state.next_sequence,
            exposure_generation: state.next_generation,
        };
        state.entries.insert(app_id.to_string(), entry.clone());
        Ok(LocalAppExposureUpdate {
            exposure: entry,
            evicted_app_id,
        })
    }

    pub async fn expose_managed_local_app(
        &self,
        conversation_id: &str,
        app_id: &str,
        pin: bool,
    ) -> Result<LocalAppExposure, McpError> {
        Ok(self
            .expose_managed_local_app_with_diff(conversation_id, app_id, pin)
            .await?
            .exposure)
    }

    /// Mark an already exposed app as recently used without hard-pinning it.
    pub async fn touch_local_app_exposure(
        &self,
        conversation_id: &str,
        app_id: &str,
    ) -> Result<LocalAppExposure, McpError> {
        if conversation_id.is_empty() || !is_local_app_id(app_id) {
            return Err(McpError::Internal(
                "invalid Local App exposure scope".into(),
            ));
        }
        let mut conversations = self.local_app_exposures.write().await;
        let state = conversations
            .get_mut(conversation_id)
            .ok_or_else(|| McpError::ToolNotFound(app_id.into()))?;
        state.next_sequence = state.next_sequence.saturating_add(1);
        let entry = state
            .entries
            .get_mut(app_id)
            .ok_or_else(|| McpError::ToolNotFound(app_id.into()))?;
        entry.last_used = state.next_sequence;
        Ok(entry.clone())
    }

    /// Change the hard-pin bit for one exposed app. Pin state is explicit and
    /// therefore advances the exposure generation independently of catalog or
    /// authoring revisions.
    pub async fn pin_local_app_exposure(
        &self,
        conversation_id: &str,
        app_id: &str,
        pinned: bool,
    ) -> Result<LocalAppExposure, McpError> {
        if conversation_id.is_empty() || !is_local_app_id(app_id) {
            return Err(McpError::Internal(
                "invalid Local App exposure scope".into(),
            ));
        }
        let mut conversations = self.local_app_exposures.write().await;
        let state = conversations
            .get_mut(conversation_id)
            .ok_or_else(|| McpError::ToolNotFound(app_id.into()))?;
        let changed = state
            .entries
            .get(app_id)
            .map(|entry| entry.pinned != pinned)
            .ok_or_else(|| McpError::ToolNotFound(app_id.into()))?;
        if changed {
            state.next_generation = state.next_generation.saturating_add(1);
        }
        let entry = state.entries.get_mut(app_id).expect("checked above");
        if changed {
            entry.exposure_generation = state.next_generation;
        }
        entry.pinned = pinned;
        Ok(entry.clone())
    }

    /// Begin one call through an exposed app. The registry rejects calls
    /// instead of queueing them without bound; callers must release the lease
    /// with [`Self::end_local_app_call`] on completion/cancellation.
    pub async fn begin_local_app_call(
        &self,
        conversation_id: &str,
        app_id: &str,
    ) -> Result<LocalAppExposure, McpError> {
        if conversation_id.is_empty() || !is_local_app_id(app_id) {
            return Err(McpError::Internal(
                "invalid Local App exposure scope".into(),
            ));
        }
        let mut conversations = self.local_app_exposures.write().await;
        let state = conversations
            .get_mut(conversation_id)
            .ok_or_else(|| McpError::ToolNotFound(app_id.into()))?;
        let total_in_flight: usize = state.entries.values().map(|entry| entry.in_flight).sum();
        let entry = state
            .entries
            .get_mut(app_id)
            .ok_or_else(|| McpError::ToolNotFound(app_id.into()))?;
        if entry.in_flight >= LOCAL_APP_MAX_IN_FLIGHT_PER_APP
            || total_in_flight >= LOCAL_APP_MAX_IN_FLIGHT_PER_CONVERSATION
        {
            return Err(McpError::Internal(
                "rate_limited: retry after 1000ms".into(),
            ));
        }
        entry.in_flight += 1;
        state.next_sequence = state.next_sequence.saturating_add(1);
        entry.last_used = state.next_sequence;
        Ok(entry.clone())
    }

    /// Release a call lease. Releasing an unknown lease is intentionally
    /// idempotent so timeout/cancel cleanup cannot turn into a second error.
    pub async fn end_local_app_call(&self, conversation_id: &str, app_id: &str) {
        let mut conversations = self.local_app_exposures.write().await;
        if let Some(state) = conversations.get_mut(conversation_id) {
            if let Some(entry) = state.entries.get_mut(app_id) {
                entry.in_flight = entry.in_flight.saturating_sub(1);
            }
        }
    }

    /// Snapshot the logical exposure metadata for one conversation in recency
    /// order. Tool DTOs are intentionally not part of this API.
    pub async fn local_app_exposures(&self, conversation_id: &str) -> Vec<LocalAppExposure> {
        let conversations = self.local_app_exposures.read().await;
        let Some(state) = conversations.get(conversation_id) else {
            return Vec::new();
        };
        let mut entries: Vec<LocalAppExposure> = state.entries.values().cloned().collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.last_used));
        entries
    }

    /// Return one refresh request for every catalog currently advertised by
    /// every SHARED connected/cached server. Consumers use this to recover
    /// deterministically after a lagged broadcast receiver instead of leaving a
    /// missed `list_changed` notification stale until the server happens to
    /// emit another one.
    pub async fn catalog_refresh_snapshot(&self) -> Vec<McpCatalogChanged> {
        let conns = self.connections.read().await;
        let mut server_names: Vec<&String> = conns.keys().collect();
        server_names.sort();
        let mut changes = Vec::new();
        for server_name in server_names {
            let Some(state) = conns.get(server_name) else {
                continue;
            };
            if state.config().name != *server_name {
                continue;
            }
            changes.extend(Self::active_catalog_snapshot(server_name, state));
        }
        changes
    }

    async fn current_catalog_snapshot_for_connection(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
    ) -> Vec<McpCatalogChanged> {
        let conns = self.connections.read().await;
        let Some(state) = conns.get(server_name) else {
            return Vec::new();
        };
        if state.config().name != server_name {
            return Vec::new();
        }
        Self::active_catalog_snapshot(server_name, state)
            .into_iter()
            .filter(|change| change.connection_id == connection_id)
            .collect()
    }

    async fn publish_catalog_snapshot_for_connection(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
        telemetry_cause: Option<&'static str>,
    ) {
        for mut change in self
            .current_catalog_snapshot_for_connection(server_name, connection_id)
            .await
        {
            change.telemetry_cause = telemetry_cause;
            let _ = self.catalog_changes.send(change);
        }
    }

    fn active_catalog_snapshot(
        server_name: &str,
        state: &McpConnectionState,
    ) -> Vec<McpCatalogChanged> {
        let (connection_id, capabilities) = match state {
            McpConnectionState::Connected {
                connection_id,
                capabilities,
                ..
            }
            | McpConnectionState::Cached {
                connection_id,
                capabilities,
                ..
            } => (*connection_id, capabilities),
            _ => return Vec::new(),
        };
        let mut changes = Vec::new();
        for (supported, kind) in [
            (capabilities.tools, McpCatalogKind::Tools),
            (capabilities.prompts, McpCatalogKind::Prompts),
            (capabilities.resources, McpCatalogKind::Resources),
        ] {
            if supported {
                changes.push(McpCatalogChanged {
                    server_name: server_name.to_string(),
                    connection_id,
                    retired_connection_id: None,
                    kind,
                    telemetry_cause: None,
                });
            }
        }
        changes
    }

    /// Re-query the catalog named by `change` and atomically replace only that
    /// slice of the connected-state snapshot. A stale connection generation is
    /// ignored. The old catalog remains intact on RPC/decode failure.
    pub async fn refresh_catalog(
        &self,
        change: &McpCatalogChanged,
    ) -> Result<Option<McpConnectionId>, McpError> {
        let (current_id, supports_kind, previous_count) = {
            let conns = self.connections.read().await;
            match conns.get(&change.server_name) {
                Some(McpConnectionState::Connected {
                    connection_id,
                    capabilities,
                    tools,
                    prompts,
                    resources,
                    ..
                }) if *connection_id == change.connection_id => {
                    let previous_count = match change.kind {
                        McpCatalogKind::Tools => Some(tools.len()),
                        McpCatalogKind::Prompts => Some(prompts.len()),
                        McpCatalogKind::Resources => Some(resources.len()),
                    };
                    let supports_kind = match change.kind {
                        McpCatalogKind::Tools => capabilities.tools,
                        McpCatalogKind::Prompts => capabilities.prompts,
                        McpCatalogKind::Resources => capabilities.resources,
                    };
                    (*connection_id, supports_kind, previous_count)
                }
                Some(McpConnectionState::Cached { connection_id, .. })
                    if *connection_id == change.connection_id =>
                {
                    // §11 Stage 2/3 — a `Cached` server has no live
                    // connection to re-query: the cached catalog IS the
                    // freshest thing this port has for it. Report success
                    // unchanged so lag recovery can rebuild the model-facing
                    // partition from the last-known cached snapshot.
                    return Ok(Some(*connection_id));
                }
                _ => return Ok(None),
            }
        };
        if !supports_kind {
            return Ok(Some(current_id));
        }
        let Some(client) = self.get_client(&change.server_name).await else {
            return Err(McpError::Internal(format!(
                "MCP server \"{}\" has no live client for catalog refresh",
                change.server_name
            )));
        };

        enum Refreshed {
            Tools(Vec<platform_api::McpToolDto>),
            Prompts(Vec<platform_api::McpPromptDto>),
            Resources(Vec<platform_api::McpResourceDto>),
        }
        let refreshed = match change.kind {
            McpCatalogKind::Tools => client
                .list_tools()
                .await
                .map(Refreshed::Tools)
                .map_err(|e| McpError::Internal(e.to_string()))?,
            McpCatalogKind::Prompts => client
                .list_prompts()
                .await
                .map(Refreshed::Prompts)
                .map_err(|e| McpError::Internal(e.to_string()))?,
            McpCatalogKind::Resources => client
                .list_resources()
                .await
                .map(Refreshed::Resources)
                .map_err(|e| McpError::Internal(e.to_string()))?,
        };

        let mut conns = self.connections.write().await;
        let Some(McpConnectionState::Connected {
            connection_id,
            tools,
            prompts,
            resources,
            ..
        }) = conns.get_mut(&change.server_name)
        else {
            return Ok(None);
        };
        if *connection_id != current_id {
            return Ok(None);
        }
        let new_count = match refreshed {
            Refreshed::Tools(next) => {
                let new_count = next.len();
                *tools = next;
                new_count
            }
            Refreshed::Prompts(next) => {
                let new_count = next.len();
                *prompts = next;
                new_count
            }
            Refreshed::Resources(next) => {
                let new_count = next.len();
                *resources = next;
                new_count
            }
        };
        if let Some(cause) = change.telemetry_cause {
            emit_list_changed(&list_changed_payload(
                &change.server_name,
                change.kind,
                cause,
                previous_count,
                Some(new_count),
            ));
        }
        Ok(Some(current_id))
    }

    fn spawn_catalog_change_listener(
        &self,
        server_name: String,
        connection_id: McpConnectionId,
        connection: Arc<jsonrpc::Connection>,
        negotiated: platform_api::McpNegotiatedProtocol,
        capabilities: ServerCapabilitiesDto,
        open_telemetry: Option<ModernListenOpenTelemetry>,
    ) {
        let registry = self.clone_for_background();
        let notifications = connection.notifications();
        tokio::spawn(async move {
            #[cfg(test)]
            maybe_pause_catalog_change_listener_for_test().await;
            if negotiated.era == platform_api::McpProtocolEra::Modern {
                registry
                    .run_modern_catalog_change_listener(
                        server_name,
                        connection_id,
                        connection,
                        notifications,
                        capabilities,
                        negotiated,
                        open_telemetry.unwrap_or(ModernListenOpenTelemetry {
                            outcome: telemetry::tengu::mcp::ListenReopenOutcome::OpenedFromZero,
                            attempts: 0,
                            trigger: telemetry::tengu::mcp::ListenReopenTrigger::Connect,
                        }),
                    )
                    .await;
                return;
            }

            let mut notifications = notifications;
            loop {
                let notification = match notifications.recv().await {
                    Ok(notification) => notification,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        notifications = connection.notifications();
                        registry
                            .publish_catalog_snapshot_for_connection(
                                &server_name,
                                connection_id,
                                Some(LISTEN_REOPEN_CAUSE),
                            )
                            .await;
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                forward_catalog_change(
                    &registry.catalog_changes,
                    &server_name,
                    connection_id,
                    &notification.method,
                    Some("notification"),
                );
            }
        });
    }

    async fn run_modern_catalog_change_listener(
        &self,
        server_name: String,
        connection_id: McpConnectionId,
        connection: Arc<jsonrpc::Connection>,
        mut notifications: broadcast::Receiver<jsonrpc::Notification>,
        capabilities: ServerCapabilitiesDto,
        negotiated: platform_api::McpNegotiatedProtocol,
        open_telemetry: ModernListenOpenTelemetry,
    ) {
        let Some(filter) = modern_listen_notifications_filter(&capabilities) else {
            return;
        };
        let listen = match connection.start_call_unbounded(
            "subscriptions/listen",
            modern_listen_request_params(&negotiated.version, filter),
        ) {
            Ok(listen) => listen,
            Err(_) => {
                #[cfg(test)]
                notify_catalog_change_listener_closed_for_test();
                self.handle_modern_catalog_listener_end(
                    &server_name,
                    connection_id,
                    telemetry::tengu::mcp::ListenReopenTrigger::Remote,
                )
                .await;
                return;
            }
        };
        let subscription_id = listen.id().clone();
        let completion = listen.wait_value();
        tokio::pin!(completion);

        loop {
            tokio::select! {
                completion = &mut completion => {
                    #[cfg(test)]
                    notify_catalog_change_listener_closed_for_test();
                    let trigger = if completion.is_ok() {
                        telemetry::tengu::mcp::ListenReopenTrigger::Graceful
                    } else {
                        telemetry::tengu::mcp::ListenReopenTrigger::Remote
                    };
                    self.handle_modern_catalog_listener_end(&server_name, connection_id, trigger)
                        .await;
                    return;
                }
                notification = notifications.recv() => {
                    match notification {
                        Ok(notification) => {
                            if !notification_matches_subscription(&notification, &subscription_id) {
                                continue;
                            }
                            if notification.method == "notifications/subscriptions/acknowledged" {
                                self.record_listener_open(&server_name, open_telemetry)
                                    .await;
                                emit_listen_reopen(&listen_reopen_payload(
                                    &server_name,
                                    open_telemetry.outcome,
                                    open_telemetry.attempts,
                                    open_telemetry.trigger,
                                ));
                                continue;
                            }
                            forward_catalog_change(
                                &self.catalog_changes,
                                &server_name,
                                connection_id,
                                &notification.method,
                                Some("notification"),
                            );
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            self.publish_catalog_snapshot_for_connection(
                                &server_name,
                                connection_id,
                                Some(LISTEN_REOPEN_CAUSE),
                            )
                            .await;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            #[cfg(test)]
                            notify_catalog_change_listener_closed_for_test();
                            self.handle_modern_catalog_listener_end(
                                &server_name,
                                connection_id,
                                telemetry::tengu::mcp::ListenReopenTrigger::Remote,
                            )
                            .await;
                            return;
                        }
                    }
                }
            }
        }
    }

    async fn handle_modern_catalog_listener_end(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
        trigger: telemetry::tengu::mcp::ListenReopenTrigger,
    ) {
        let (mut delay_index, reopen_count) = self.prepare_modern_reopen_cycle(server_name).await;
        if reopen_count >= LISTENER_REOPEN_MAX_ATTEMPTS_PER_WINDOW {
            emit_listen_reopen(&listen_reopen_payload(
                server_name,
                telemetry::tengu::mcp::ListenReopenOutcome::BudgetExhausted,
                reopen_count as u32,
                trigger,
            ));
            emit_listen_reopen(&listen_reopen_payload(
                server_name,
                telemetry::tengu::mcp::ListenReopenOutcome::Parked,
                reopen_count as u32,
                trigger,
            ));
            if !self
                .wait_listener_reopen_park(server_name, connection_id)
                .await
            {
                emit_listen_reopen(&listen_reopen_payload(
                    server_name,
                    telemetry::tengu::mcp::ListenReopenOutcome::GaveUp,
                    0,
                    trigger,
                ));
                return;
            }
            delay_index = 0;
        }

        let mut last_error = None;
        for attempt in 1..=LISTENER_REOPEN_RETRY_DELAYS.len() {
            let capped_index = delay_index
                .saturating_add(attempt - 1)
                .min(LISTENER_REOPEN_RETRY_DELAYS.len() - 1);
            let mut delay = LISTENER_REOPEN_RETRY_DELAYS[capped_index];
            if attempt == 1 && trigger == telemetry::tengu::mcp::ListenReopenTrigger::Graceful {
                delay += LISTENER_REOPEN_GRACEFUL_DELAY;
            }
            if !self
                .wait_listener_reopen_delay(server_name, connection_id, delay)
                .await
            {
                emit_listen_reopen(&listen_reopen_payload(
                    server_name,
                    telemetry::tengu::mcp::ListenReopenOutcome::GaveUp,
                    (attempt - 1) as u32,
                    trigger,
                ));
                return;
            }
            match self
                .reopen_catalog_listener_generation(
                    server_name,
                    connection_id,
                    trigger,
                    attempt as u32,
                    capped_index,
                )
                .await
            {
                Ok(true) => return,
                Ok(false) => {
                    emit_listen_reopen(&listen_reopen_payload(
                        server_name,
                        telemetry::tengu::mcp::ListenReopenOutcome::GaveUp,
                        attempt as u32,
                        trigger,
                    ));
                    return;
                }
                Err(error) => {
                    last_error = Some(error);
                }
            }
        }

        let reason =
            last_error.unwrap_or_else(|| McpError::Connection("listen reopen failed".into()));
        self.mark_listener_generation_disconnected(server_name, connection_id, &reason)
            .await;
        emit_listen_reopen(&listen_reopen_payload(
            server_name,
            telemetry::tengu::mcp::ListenReopenOutcome::GaveUp,
            LISTENER_REOPEN_RETRY_DELAYS.len() as u32,
            trigger,
        ));
    }

    async fn prepare_modern_reopen_cycle(&self, server_name: &str) -> (usize, usize) {
        let now = tokio::time::Instant::now();
        let mut states = self.listener_reopen_state.write().await;
        let state = states.entry(server_name.to_string()).or_default();
        if state.opened_at.take().is_some_and(|opened| {
            now.saturating_duration_since(opened) >= LISTENER_REOPEN_STABLE_RESET
        }) {
            state.delay_index = 0;
        }
        state
            .reopened_at
            .retain(|reopened| now.saturating_duration_since(*reopened) < LISTENER_REOPEN_WINDOW);
        (state.delay_index, state.reopened_at.len())
    }

    async fn record_listener_open(
        &self,
        server_name: &str,
        open_telemetry: ModernListenOpenTelemetry,
    ) {
        let now = tokio::time::Instant::now();
        let mut states = self.listener_reopen_state.write().await;
        let state = states.entry(server_name.to_string()).or_default();
        state.opened_at = Some(now);
        match open_telemetry.outcome {
            telemetry::tengu::mcp::ListenReopenOutcome::OpenedFromZero => {
                state.delay_index = 0;
            }
            telemetry::tengu::mcp::ListenReopenOutcome::Reopened => {
                state.delay_index = state
                    .delay_index
                    .saturating_add(1)
                    .min(LISTENER_REOPEN_RETRY_DELAYS.len().saturating_sub(1));
                state.reopened_at.retain(|reopened| {
                    now.saturating_duration_since(*reopened) < LISTENER_REOPEN_WINDOW
                });
                state.reopened_at.push(now);
            }
            telemetry::tengu::mcp::ListenReopenOutcome::GaveUp
            | telemetry::tengu::mcp::ListenReopenOutcome::BudgetExhausted
            | telemetry::tengu::mcp::ListenReopenOutcome::Parked => {}
        }
    }

    async fn wait_listener_reopen_delay(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
        delay: Duration,
    ) -> bool {
        tokio::select! {
            _ = tokio::time::sleep(delay) => true,
            _ = self.wait_until_listener_cancelled(server_name, connection_id) => false,
        }
    }

    async fn wait_listener_reopen_park(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
    ) -> bool {
        let jitter = listener_reopen_park_jitter();
        let park = LISTENER_REOPEN_PARK.mul_f64(jitter);
        let deadline = tokio::time::Instant::now() + park;
        loop {
            if !self
                .is_listener_generation_current(server_name, connection_id)
                .await
            {
                return false;
            }
            let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now())
            else {
                return true;
            };
            tokio::time::sleep(std::cmp::min(remaining, LISTENER_REOPEN_PARK_POLL)).await;
        }
    }

    async fn wait_until_listener_cancelled(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
    ) {
        loop {
            if !self
                .is_listener_generation_current(server_name, connection_id)
                .await
            {
                return;
            }
            tokio::time::sleep(LISTENER_REOPEN_PARK_POLL).await;
        }
    }

    async fn is_listener_generation_current(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
    ) -> bool {
        let conns = self.connections.read().await;
        matches!(
            conns.get(server_name),
            Some(McpConnectionState::Connected {
                connection_id: current,
                config,
                ..
            }) if *current == connection_id && config.name == server_name && !config.disabled
        )
    }

    async fn reopen_catalog_listener_generation(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
        trigger: telemetry::tengu::mcp::ListenReopenTrigger,
        attempts: u32,
        delay_index: usize,
    ) -> Result<bool, McpError> {
        let lifecycle = self.lifecycle_lock(server_name);
        let _guard = lifecycle.lock().await;
        let Some(config) = ({
            let conns = self.connections.read().await;
            match conns.get(server_name) {
                Some(McpConnectionState::Connected {
                    connection_id: current,
                    config,
                    ..
                }) if *current == connection_id
                    && config.name == server_name
                    && !config.disabled =>
                {
                    Some(config.clone())
                }
                _ => None,
            }
        }) else {
            return Ok(false);
        };

        let negotiation_mode = crate::protocol_negotiation::resolve_for_spec_with_transport(
            &config.spec,
            config.metadata.transport.as_deref(),
            mcp_connection_timeout().as_millis() as u64,
        );
        let discovery = self
            .discover_live_connection(&config, negotiation_mode)
            .await?;
        let new_connection_id = self
            .install_live_discovery(
                server_name.to_string(),
                config,
                discovery,
                Some(connection_id),
                None,
                Some(ModernListenOpenTelemetry {
                    outcome: telemetry::tengu::mcp::ListenReopenOutcome::Reopened,
                    attempts,
                    trigger,
                }),
            )
            .await?;
        self.set_listener_reopen_delay_index(server_name, delay_index)
            .await;
        self.disconnect_or_schedule_cleanup(connection_id).await;
        let _ = new_connection_id;
        Ok(true)
    }

    async fn set_listener_reopen_delay_index(&self, server_name: &str, delay_index: usize) {
        let mut states = self.listener_reopen_state.write().await;
        let state = states.entry(server_name.to_string()).or_default();
        state.delay_index = delay_index;
        state.opened_at = None;
    }

    async fn mark_listener_generation_disconnected(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
        error: &McpError,
    ) {
        let Some(config) = ({
            let conns = self.connections.read().await;
            match conns.get(server_name) {
                Some(McpConnectionState::Connected {
                    connection_id: current,
                    config,
                    ..
                }) if *current == connection_id => Some(config.clone()),
                _ => None,
            }
        }) else {
            return;
        };
        self.connections.write().await.insert(
            server_name.to_string(),
            McpConnectionState::Disconnected {
                config: config.clone(),
                last_error: Some(error.to_string()),
            },
        );
        self.clients.write().await.shift_remove(server_name);
        self.clear_prompt_predecessors_for_key(server_name).await;
        self.emit_retire_event_if_shared(&config, server_name, connection_id)
            .await;
    }

    async fn publish_listener_reopen_catalog_changes(
        &self,
        server_name: &str,
        connection_id: McpConnectionId,
        retired_connection_id: Option<McpConnectionId>,
        capabilities: &ServerCapabilitiesDto,
    ) {
        for kind in [
            McpCatalogKind::Tools,
            McpCatalogKind::Prompts,
            McpCatalogKind::Resources,
        ] {
            let supported = match kind {
                McpCatalogKind::Tools => capabilities.tools,
                McpCatalogKind::Prompts => capabilities.prompts,
                McpCatalogKind::Resources => capabilities.resources,
            };
            if !supported {
                continue;
            }
            self.publish_catalog_change(McpCatalogChanged {
                server_name: server_name.to_string(),
                connection_id,
                retired_connection_id: retired_connection_id
                    .filter(|_| kind == McpCatalogKind::Tools),
                kind,
                telemetry_cause: Some(LISTEN_REOPEN_CAUSE),
            })
            .await;
        }
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
    ) -> Result<platform_api::McpToolResultDto, crate::client::McpClientError> {
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
        Vec<platform_api::McpToolDto>,
        Vec<platform_api::McpResourceDto>,
        Vec<platform_api::McpResourceTemplateDto>,
        Vec<platform_api::McpPromptDto>,
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

    async fn record_discovery_cache_refresh_failure_locked(
        &self,
        key: &str,
        cached_connection_id: McpConnectionId,
        config: &McpServerConfig,
        partition: Option<&DiscoveryCachePartition>,
    ) {
        let Some(partition) = partition else {
            return;
        };
        let still_current = {
            let conns = self.connections.read().await;
            match conns.get(key) {
                Some(McpConnectionState::Cached {
                    connection_id,
                    config: current_config,
                    ..
                }) => {
                    *connection_id == cached_connection_id
                        && Self::same_config_snapshot(current_config, config)
                }
                _ => false,
            }
        };
        if !still_current {
            return;
        }
        self.record_discovery_cache_refresh_failure(config, partition);
    }

    async fn discovery_cache_partition_for(
        &self,
        config: &McpServerConfig,
        negotiation_mode: crate::protocol_negotiation::NegotiationMode,
    ) -> Result<DiscoveryCachePartition, crate::discovery_cache::MissReason> {
        let grant_provenance = self.current_grant_provenance(config).await?;
        self.discovery_cache_partition_for_grant(
            config,
            negotiation_mode,
            grant_provenance.as_ref(),
        )
        .await
    }

    async fn discovery_cache_partition_for_grant(
        &self,
        config: &McpServerConfig,
        negotiation_mode: crate::protocol_negotiation::NegotiationMode,
        grant_provenance: Option<&GrantProvenance>,
    ) -> Result<DiscoveryCachePartition, crate::discovery_cache::MissReason> {
        // Agent catalogs are safe to cache only when their stable source is
        // present.  A missing source fails closed rather than sharing an
        // agent-scoped catalog under the plain server name/spec.
        if config.scope == ConfigScope::Agent && config.metadata.agent_source.is_none() {
            return Err(crate::discovery_cache::MissReason::NoFingerprint);
        }
        let Some(grant_provenance) = grant_provenance else {
            return Err(crate::discovery_cache::MissReason::NoFingerprint);
        };
        let logical_key = crate::discovery_cache::logical_cache_key(config);
        let expected_era = match negotiation_mode {
            crate::protocol_negotiation::NegotiationMode::Auto { .. } => "modern",
            crate::protocol_negotiation::NegotiationMode::Legacy => "legacy",
        };
        let partition_key = crate::discovery_cache::partition_key_for_era(
            &logical_key,
            &grant_provenance.fingerprint,
            expected_era,
        );
        Ok(DiscoveryCachePartition {
            logical_key,
            partition_key,
            expected_era,
            negotiation_mode,
        })
    }

    async fn current_grant_provenance(
        &self,
        config: &McpServerConfig,
    ) -> Result<Option<GrantProvenance>, crate::discovery_cache::MissReason> {
        let has_oauth = matches!(
            &config.spec,
            McpTransportSpec::Sse { oauth: Some(_), .. }
                | McpTransportSpec::Http { oauth: Some(_), .. }
        );
        if !has_oauth {
            return Ok(Some(GrantProvenance::unbound()));
        }
        let Some(deps) = &self.oauth else {
            return Ok(None);
        };
        let key = oauth::server_key(&config.name, &config.spec);
        let grant_token = oauth::discovery_cache_grant_token(&deps.storage, &key)
            .await
            .map_err(|_| crate::discovery_cache::MissReason::NoFingerprint)?;
        let Some(grant_token) = grant_token else {
            return Ok(None);
        };
        Ok(Some(GrantProvenance::from_grant_token(&grant_token, true)))
    }

    async fn discovery_cache_secret_candidates_for(
        &self,
        config: &McpServerConfig,
    ) -> Result<Vec<String>, ()> {
        let mut candidates = Self::config_secret_candidates(config);
        if let Some(deps) = &self.oauth {
            if matches!(
                config.spec,
                McpTransportSpec::Sse { .. } | McpTransportSpec::Http { .. }
            ) {
                let server_key = oauth::server_key(&config.name, &config.spec);
                let stored = oauth::load_tokens(&deps.storage, &server_key)
                    .await
                    .map_err(|_| ())?;
                if let Some(stored) = stored {
                    candidates.push(stored.access_token);
                    if let Some(refresh) = stored.refresh_token {
                        candidates.push(refresh);
                    }
                    if let Some(client_secret) = stored.client_secret {
                        candidates.push(client_secret);
                    }
                }
            }
        }
        candidates.sort();
        candidates.dedup();
        Ok(candidates)
    }

    fn config_secret_candidates(config: &McpServerConfig) -> Vec<String> {
        let mut candidates = Vec::new();
        let push_secret = |candidates: &mut Vec<String>, value: &str| {
            let value = value.trim();
            if value.len() >= 8 {
                candidates.push(value.to_string());
            }
        };
        let push_secret_variants = |candidates: &mut Vec<String>, value: &str| {
            push_secret(candidates, value);
            for component in
                value.split(|ch: char| ch.is_ascii_whitespace() || matches!(ch, ',' | ';'))
            {
                let component = component.trim_matches(|ch| matches!(ch, '"' | '\''));
                push_secret(candidates, component);
                if let Some((_, suffix)) = component.split_once('=') {
                    push_secret(
                        candidates,
                        suffix.trim_matches(|ch| matches!(ch, '"' | '\'')),
                    );
                }
            }
        };
        let maybe_push_url_credentials = |candidates: &mut Vec<String>, url: &str| {
            if let Ok(parsed) = url::Url::parse(url) {
                if !parsed.username().is_empty() {
                    push_secret(candidates, parsed.username());
                }
                if let Some(password) = parsed.password() {
                    push_secret(candidates, password);
                }
                for (name, value) in parsed.query_pairs() {
                    let lower_name = name.to_ascii_lowercase();
                    let suspicious_name = [
                        "auth", "token", "key", "secret", "cookie", "session", "sig", "pass",
                        "cred", "bearer",
                    ]
                    .iter()
                    .any(|needle| lower_name.contains(needle));
                    let selector_like = value.len() <= 32
                        && value.bytes().all(|b| {
                            b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_".contains(&b)
                        });
                    if suspicious_name || !selector_like {
                        push_secret(candidates, &value);
                    }
                }
                for segment in parsed.path_segments().into_iter().flatten() {
                    let high_entropy = segment.len() >= 24
                        && segment
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"._~+/=%-".contains(&b))
                        && segment.bytes().any(|b| b.is_ascii_alphabetic())
                        && (segment.bytes().any(|b| b.is_ascii_digit())
                            || (segment.bytes().any(|b| b.is_ascii_lowercase())
                                && segment.bytes().any(|b| b.is_ascii_uppercase())));
                    if high_entropy {
                        push_secret(candidates, segment);
                    }
                }
            }
        };
        let maybe_push_headers =
            |candidates: &mut Vec<String>, headers: &platform_api::McpHeaders| {
                for (name, value) in headers {
                    let lower_name = name.to_ascii_lowercase();
                    let lower_value = value.trim().to_ascii_lowercase();
                    let suspicious_name = [
                        "auth", "token", "key", "secret", "cookie", "session", "sig", "pass",
                        "cred", "bearer",
                    ]
                    .iter()
                    .any(|needle| lower_name.contains(needle));
                    let suspicious_value =
                        lower_value.starts_with("bearer ") || lower_value.starts_with("basic ");
                    let exempt_name = matches!(
                        lower_name.as_str(),
                        "origin" | "referer" | "host" | "user-agent"
                    ) || lower_name.ends_with("-id")
                        || lower_name.ends_with("-version")
                        || lower_name.ends_with("-name");
                    if suspicious_name || suspicious_value || !exempt_name {
                        push_secret_variants(candidates, value);
                    }
                }
            };
        match &config.spec {
            McpTransportSpec::Sse { url, headers, .. }
            | McpTransportSpec::Http { url, headers, .. }
            | McpTransportSpec::WebSocket { url, headers, .. } => {
                maybe_push_url_credentials(&mut candidates, url);
                maybe_push_headers(&mut candidates, headers);
            }
            McpTransportSpec::WsIde {
                url, auth_token, ..
            } => {
                maybe_push_url_credentials(&mut candidates, url);
                if let Some(auth_token) = auth_token {
                    push_secret(&mut candidates, auth_token);
                }
            }
            McpTransportSpec::SseIde {
                url, auth_token, ..
            } => {
                maybe_push_url_credentials(&mut candidates, url);
                if let Some(auth_token) = auth_token {
                    push_secret(&mut candidates, auth_token);
                }
            }
            McpTransportSpec::Stdio { env, .. } => {
                for value in env.values() {
                    push_secret(&mut candidates, value);
                }
            }
            McpTransportSpec::InProcess { .. } | McpTransportSpec::SdkControl { .. } => {}
        }
        candidates
    }

    fn discovery_cache_entry_reflects_secret(serialized: &str, candidates: &[String]) -> bool {
        let contains = |candidate: &str| {
            serialized.contains(candidate)
                || serde_json::to_string(candidate)
                    .ok()
                    .and_then(|escaped| {
                        escaped
                            .strip_prefix('"')
                            .and_then(|s| s.strip_suffix('"'))
                            .map(str::to_string)
                    })
                    .is_some_and(|escaped| serialized.contains(&escaped))
        };
        candidates.iter().any(|candidate| {
            if candidate.is_empty() {
                return false;
            }
            if contains(candidate) {
                return true;
            }
            let encoded: String =
                url::form_urlencoded::byte_serialize(candidate.as_bytes()).collect();
            if encoded == *candidate {
                return false;
            }
            if contains(&encoded) {
                return true;
            }
            let mut lower_percent_hex = encoded.into_bytes();
            let mut index = 0;
            while index + 2 < lower_percent_hex.len() {
                if lower_percent_hex[index] == b'%' {
                    lower_percent_hex[index + 1].make_ascii_lowercase();
                    lower_percent_hex[index + 2].make_ascii_lowercase();
                    index += 3;
                } else {
                    index += 1;
                }
            }
            String::from_utf8(lower_percent_hex)
                .ok()
                .is_some_and(|encoded| contains(&encoded))
        })
    }

    /// §11 — before dialing, compute what the discovery cache decides for
    /// this server (oracle `cot`/`me`). Returns `None` only when no store is
    /// wired (every pre-Stage-2 caller's behavior: never consult, never
    /// serve, never emit). With a store wired this ALWAYS returns `Some`,
    /// including a `Miss` — the caller decides what to do with each variant
    /// (Stage 2: serve `Fresh`/`Stale` without dialing; emit telemetry for a
    /// `Miss` the oracle's `Ko` gate reports, then dial live).
    async fn discovery_cache_decision_for(
        &self,
        config: &McpServerConfig,
        negotiation_mode: crate::protocol_negotiation::NegotiationMode,
    ) -> Option<DiscoveryCacheConsult> {
        let store = self.discovery_cache_store.as_ref()?;
        let feature_enabled = crate::discovery_cache::feature_enabled();
        let consult = match crate::discovery_cache::cache_gate_with_metadata(
            &config.spec,
            config.discovery_cache,
            feature_enabled,
            &config.metadata,
        ) {
            Some(reason) => {
                if reason.purges_existing_entry() {
                    let _ = store.purge_server_family(&config.name);
                }
                DiscoveryCacheConsult {
                    decision: crate::discovery_cache::Decision::Miss {
                        reason: reason.miss_reason(),
                    },
                    partition: None,
                }
            }
            None => {
                let partition = match self
                    .discovery_cache_partition_for(config, negotiation_mode)
                    .await
                {
                    Ok(partition) => partition,
                    Err(reason) => {
                        return Some(DiscoveryCacheConsult {
                            decision: crate::discovery_cache::Decision::Miss { reason },
                            partition: None,
                        });
                    }
                };
                let lookup = store.load_partitioned_for_era(
                    &partition.logical_key,
                    &partition.partition_key,
                    partition.expected_era,
                );
                let policy = crate::discovery_cache::DiscoveryCachePolicy::from_env(
                    crate::discovery_cache::now_ms(),
                );
                DiscoveryCacheConsult {
                    decision: crate::discovery_cache::decide_with_metadata(
                        &config.spec,
                        config.discovery_cache,
                        feature_enabled,
                        lookup,
                        policy,
                        &config.metadata,
                    ),
                    partition: Some(partition),
                }
            }
        };
        Some(consult)
    }

    /// §11 Stage 2 — serve a `Fresh`/`Stale` discovery-cache hit WITHOUT
    /// dialing: install a [`McpConnectionState::Cached`] under `key` carrying
    /// the entry's full catalog, emit `tengu_mcp_discovery_source` with
    /// `source` `"cache_fresh"`/`"cache_stale"` and the real `entryAgeMs`
    /// (oracle @182536408's hit branch), and — for a session-level (not
    /// agent-scoped) server — fan the change out on [`Self::catalog_changes`]
    /// exactly like a live connect does, so a mid-session cache hit (a
    /// reconnect that resolves `Fresh`/`Stale` instead of `Miss`) still
    /// reaches the live `ToolRegistry` without a restart. Returns the freshly
    /// allocated [`McpConnectionId`] — see [`McpConnectionState::Cached`]'s
    /// doc for why it is safe to mint one with nothing live behind it.
    async fn serve_discovery_cache_hit(
        &self,
        config: &McpServerConfig,
        key: &str,
        entry: crate::discovery_cache::DiscoveryCacheEntry,
        age_ms: u64,
        is_fresh: bool,
        operation_guard: Option<&McpOperationGuard>,
    ) -> Result<McpConnectionId, McpError> {
        if operation_guard.is_some_and(|guard| !guard()) {
            return Err(operation_guard_rejected());
        }
        let connection_id = McpConnectionId::new();
        let negotiated = negotiated_protocol_from_cache_entry(&entry);
        let invalidated_slot = self.invalidate_lazy_upgrade_slot(key).await;
        Self::finish_invalidated_lazy_upgrade_slot(invalidated_slot.as_ref());
        if operation_guard.is_some_and(|guard| !guard()) {
            return Err(operation_guard_rejected());
        }
        self.clear_prompt_predecessors_for_key(key).await;
        let mut connections = self.connections.write().await;
        if operation_guard.is_some_and(|guard| !guard()) {
            return Err(operation_guard_rejected());
        }
        connections.insert(
            key.to_string(),
            McpConnectionState::Cached {
                config: config.clone(),
                connection_id,
                capabilities: entry.capabilities,
                negotiated,
                tools: entry.tools,
                resources: entry.resources,
                resource_templates: entry.resource_templates,
                prompts: entry.prompts,
                cache_saved_at_ms: entry.saved_at_ms,
                age_ms,
            },
        );
        telemetry::emit_mcp_discovery_source(&telemetry::tengu::mcp::DiscoverySourcePayload {
            transport_type: telemetry::pii::Verified::assert_safe(config.spec.kind().to_string()),
            source: telemetry::pii::Verified::assert_safe(
                if is_fresh {
                    "cache_fresh"
                } else {
                    "cache_stale"
                }
                .to_string(),
            ),
            entry_age_ms: Some(age_ms),
        });
        // Same gate the live-connect tail uses (`table_key.is_none()`):
        // `key == config.name` for an ordinary session-level server (no
        // table-key namespacing applied), `false` for an agent-scoped one
        // (`agent_scope_table_key` never collides with a plain name — see its
        // doc). An agent-scoped cache hit stays private to the subagent that
        // requested it, exactly like a live agent-scoped connect.
        if key == config.name {
            self.publish_catalog_change(McpCatalogChanged {
                server_name: config.name.clone(),
                connection_id,
                retired_connection_id: None,
                kind: McpCatalogKind::Tools,
                telemetry_cause: None,
            })
            .await;
        }
        Ok(connection_id)
    }

    /// §11 write-through: after a LIVE discovery round completes
    /// (`connect_locked_inner`, right before `config`/`caps`/`tools`/…
    /// move into the `Connected` state), persist the freshly discovered
    /// catalog for a cache-ELIGIBLE server so a future connect can serve it.
    /// The authenticated grant partition is captured before catalog RPCs and
    /// re-resolved before write; a mismatch skips persistence.
    ///
    /// A gate reason [`crate::discovery_cache::CacheGateReason::purges_existing_entry`]
    /// flags instead purges any existing on-disk entry, best-effort — the
    /// write-side counterpart of the same purge the oracle's read-side `cot`
    /// performs on an `opt-out`/`headers-helper` miss.
    ///
    /// Best-effort throughout: any store I/O failure is logged and
    /// swallowed, matching the oracle's `catch(r){Z(e.name, \`Discovery
    /// cache write-through skipped: ${l(r)}\`)}` around `Wo`.
    #[allow(clippy::too_many_arguments)]
    async fn persist_or_purge_discovery_cache(
        &self,
        config: &McpServerConfig,
        captured_partition: Option<&DiscoveryCachePartition>,
        caps: &ServerCapabilitiesDto,
        tools: &[platform_api::McpToolDto],
        resources: &[platform_api::McpResourceDto],
        resource_templates: &[platform_api::McpResourceTemplateDto],
        prompts: &[platform_api::McpPromptDto],
        negotiation_mode: crate::protocol_negotiation::NegotiationMode,
        grant_provenance: Option<&GrantProvenance>,
        negotiated: Option<&platform_api::McpNegotiatedProtocol>,
    ) {
        let Some(store) = &self.discovery_cache_store else {
            return;
        };
        let feature_enabled = crate::discovery_cache::feature_enabled();
        let gate = crate::discovery_cache::cache_gate_with_metadata(
            &config.spec,
            config.discovery_cache,
            feature_enabled,
            &config.metadata,
        );
        let cache_key = crate::discovery_cache::logical_cache_key(config);
        match gate {
            None => {
                let Some(captured_partition) = captured_partition else {
                    return;
                };
                let Some(captured_grant) = grant_provenance else {
                    return;
                };
                let current_grant = if captured_grant.verify_current {
                    let Ok(Some(current_grant)) = self.current_grant_provenance(config).await
                    else {
                        return;
                    };
                    current_grant
                } else {
                    captured_grant.clone()
                };
                if current_grant != *captured_grant {
                    tracing::warn!(
                        server = %config.name,
                        "Discovery cache write-through skipped because the OAuth grant rotated during discovery"
                    );
                    return;
                }
                let Ok(partition) = self
                    .discovery_cache_partition_for_grant(
                        config,
                        negotiation_mode,
                        Some(&current_grant),
                    )
                    .await
                else {
                    return;
                };
                if &partition != captured_partition {
                    return;
                }
                let entry = crate::discovery_cache::DiscoveryCacheEntry::new(
                    cache_key,
                    crate::discovery_cache::now_ms(),
                    caps.clone(),
                    tools.to_vec(),
                    resources.to_vec(),
                    resource_templates.to_vec(),
                    prompts.to_vec(),
                )
                .with_negotiated_era(
                    negotiated
                        .map(|protocol| negotiated_era_label(protocol.era))
                        .unwrap_or("legacy"),
                );
                let Ok(serialized) = serde_json::to_string(&entry) else {
                    return;
                };
                let Ok(secret_candidates) =
                    self.discovery_cache_secret_candidates_for(config).await
                else {
                    return;
                };
                if Self::discovery_cache_entry_reflects_secret(&serialized, &secret_candidates) {
                    tracing::warn!(
                        server = %config.name,
                        "Discovery cache write-through skipped because the serialized catalog reflected secret material"
                    );
                    return;
                }
                if let Err(error) = store.store_partitioned(&entry, &partition.partition_key) {
                    tracing::warn!(
                        server = %config.name,
                        %error,
                        "Discovery cache write-through skipped"
                    );
                }
            }
            Some(reason) if reason.purges_existing_entry() => {
                if let Err(error) = store.purge_server_family(&config.name) {
                    tracing::warn!(
                        server = %config.name,
                        %error,
                        "Discovery cache purge skipped"
                    );
                }
            }
            Some(_) => {}
        }
    }

    /// Record one oracle `_6e` strike against the exact partition that served
    /// the stale catalog. Ordinary connection failures never call this path.
    /// Keeping the captured partition avoids striking a new identity partition
    /// if the remote MCP refresh grant rotates during background revalidation.
    fn record_discovery_cache_refresh_failure(
        &self,
        config: &McpServerConfig,
        partition: &DiscoveryCachePartition,
    ) {
        let Some(store) = &self.discovery_cache_store else {
            return;
        };
        if let crate::discovery_cache::EntryLookup::Found(mut entry) =
            store.load_partitioned(&partition.logical_key, &partition.partition_key)
        {
            entry.consecutive_refresh_failures =
                entry.consecutive_refresh_failures.saturating_add(1);
            if let Err(error) = store.store_partitioned(&entry, &partition.partition_key) {
                tracing::warn!(
                    server = %config.name,
                    %error,
                    "Discovery cache strike write skipped"
                );
            }
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
            platform_api::McpNegotiatedProtocol,
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
                platform_api::McpProtocolEra::Modern
            }
            crate::protocol_negotiation::NegotiationMode::Legacy => {
                platform_api::McpProtocolEra::Legacy
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
                    platform_api::McpConnectOptions {
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

    /// Resolve the spec to connect with, attaching a Bearer token for OAuth
    /// servers. Returns `(spec, Some(server_key))` for an OAuth-configured
    /// SSE/HTTP server (token loaded → refreshed-on-expiry → freshly minted via
    /// the interactive flow), or `(config.spec.clone(), None)` for static-token
    /// servers and any server when the OAuth seam is unwired.
    ///
    /// Mirrors claude-code's per-connect token resolution (`auth.ts` `tokens()`
    /// + `useManageMCPConnections` attaching the Authorization header).
    async fn resolve_oauth_spec(
        &self,
        config: &McpServerConfig,
    ) -> Result<(McpTransportSpec, Option<String>, Option<GrantProvenance>), McpError> {
        let Some(oauth_cfg) = spec_oauth(&config.spec) else {
            return Ok((config.spec.clone(), None, Some(GrantProvenance::unbound())));
        };
        let Some(deps) = &self.oauth else {
            return Ok((config.spec.clone(), None, None));
        };
        let key = oauth::server_key(&config.name, &config.spec);
        let telemetry_ctx = oauth::McpOAuthTelemetryContext::for_server(&config.name, &config.spec);

        // XAA (cross-app-access, SEP-990): when `oauth.xaa` is set, XAA is the
        // ONLY auth path — never fall through to the consent flow (auth.ts:857-
        // 900). Gated by `LINGXI_ENABLE_XAA` (mirror of CLAUDE_CODE_ENABLE_XAA);
        // a flagged server with the env unset hard-fails with actionable copy.
        if oauth_cfg.xaa == Some(true) {
            let token = self.resolve_xaa_token(config, &key, deps).await?;
            return Ok((
                inject_bearer(&config.spec, token.access_token.expose_secret()),
                Some(key),
                GrantProvenance::from_tokens(&token),
            ));
        }

        // 1. Stored token, unexpired → use it.
        // 2. Stored token, expired with a refresh token → refresh, persist.
        // 3. No usable token → run the interactive flow, persist.
        let token = match oauth::load_tokens(&deps.storage, &key).await? {
            // Unexpired stored token → use it directly.
            Some(stored) if deps.clock.now() < stored.expires_at() => stored.into_tokens(),
            // Expired stored token.
            Some(stored) => {
                match stored.refresh_token.clone() {
                    Some(refresh) => {
                        // Proactive (pre-connect) refresh: no live challenge
                        // exists yet, so discovery uses the well-known guess.
                        let meta = match oauth::discover_auth_server_metadata(
                            &deps.http,
                            spec_url(&config.spec),
                            oauth_cfg.auth_server_metadata_url.as_deref(),
                            None,
                        )
                        .await
                        {
                            Ok(meta) => meta,
                            Err(error) => {
                                oauth::emit_oauth_refresh_failure(
                                    &telemetry_ctx,
                                    oauth_refresh_failure_reason(&error),
                                );
                                return Err(error.into());
                            }
                        };
                        // Prefer the client_id the stored tokens were minted
                        // with (DCR-issued OR configured) so silent refresh
                        // re-sends it; fall back to the configured id, then ""
                        // (auth.ts clientInformation(), 1482-1506).
                        let client_id = stored
                            .client_id
                            .clone()
                            .or_else(|| oauth_cfg.client_id.clone())
                            .unwrap_or_default();
                        let refreshed = match oauth::refresh_tokens(
                            &deps.http,
                            &deps.clock,
                            &meta,
                            &client_id,
                            None, // public client — no confidential secret to send
                            &refresh,
                        )
                        .await
                        {
                            Ok(refreshed) => refreshed,
                            Err(error) => {
                                oauth::emit_oauth_refresh_failure(
                                    &telemetry_ctx,
                                    oauth_refresh_failure_reason(&error),
                                );
                                return Err(error.into());
                            }
                        };
                        oauth::save_tokens_with_telemetry(
                            &deps.storage,
                            &deps.clock,
                            &key,
                            &refreshed,
                            Some(&telemetry_ctx),
                        )
                        .await?;
                        oauth::emit_oauth_refresh_success(&telemetry_ctx);
                        refreshed
                    }
                    None => {
                        self.run_interactive_oauth(config, oauth_cfg, &key, deps, None, None)
                            .await?
                    }
                }
            }
            None => {
                self.run_interactive_oauth(config, oauth_cfg, &key, deps, None, None)
                    .await?
            }
        };

        Ok((
            inject_bearer(&config.spec, token.access_token.expose_secret()),
            Some(key),
            GrantProvenance::from_tokens(&token),
        ))
    }

    /// Re-authenticate after a 401: refresh if a refresh token is stored, else
    /// run a fresh interactive flow, then return the spec with the new Bearer.
    /// `resource_metadata_url` (§24c) is the `resource_metadata` param parsed
    /// from the triggering 401's `WWW-Authenticate` challenge, when it carried
    /// a structured one — threaded into discovery in place of the well-known
    /// guess.
    async fn reauth_oauth_spec(
        &self,
        config: &McpServerConfig,
        resource_metadata_url: Option<&str>,
    ) -> Result<(McpTransportSpec, Option<GrantProvenance>), McpError> {
        let deps = self
            .oauth
            .as_ref()
            .ok_or_else(|| McpError::OAuth("oauth seam not wired".into()))?;
        let oauth_cfg = spec_oauth(&config.spec)
            .ok_or_else(|| McpError::OAuth("server has no oauth config".into()))?;
        let key = oauth::server_key(&config.name, &config.spec);
        let telemetry_ctx = oauth::McpOAuthTelemetryContext::for_server(&config.name, &config.spec);

        // §26b delta 3: an XAA-flagged server's 401 must stay on the XAA
        // path — never fall through to the refresh-or-interactive-consent
        // logic below, matching this module's own guarantee (see
        // `resolve_oauth_spec`) that XAA is the ONLY auth path. The server
        // just rejected whatever was cached, so `force_fresh` skips reusing
        // it: a stored refresh token drives the ordinary refresh grant; its
        // absence (or rejection) drives a fresh silent IdP+AS exchange.
        // Interactive consent is never reachable from this arm.
        if oauth_cfg.xaa == Some(true) {
            let token = self
                .resolve_xaa_token_inner(config, &key, deps, true, resource_metadata_url)
                .await?;
            return Ok((
                inject_bearer(&config.spec, token.access_token.expose_secret()),
                GrantProvenance::from_tokens(&token),
            ));
        }

        let stored = oauth::load_tokens(&deps.storage, &key).await?;
        let token = match stored.as_ref().and_then(|t| t.refresh_token.clone()) {
            Some(refresh) => {
                let meta = match oauth::discover_auth_server_metadata(
                    &deps.http,
                    spec_url(&config.spec),
                    oauth_cfg.auth_server_metadata_url.as_deref(),
                    resource_metadata_url,
                )
                .await
                {
                    Ok(meta) => meta,
                    Err(error) => {
                        oauth::emit_oauth_refresh_failure(
                            &telemetry_ctx,
                            oauth_refresh_failure_reason(&error),
                        );
                        return Err(error.into());
                    }
                };
                // Prefer the persisted (DCR-issued or configured) client_id so
                // refresh re-sends it (auth.ts clientInformation(), 1482-1506).
                let client_id = stored
                    .as_ref()
                    .and_then(|t| t.client_id.clone())
                    .or_else(|| oauth_cfg.client_id.clone())
                    .unwrap_or_default();
                match oauth::refresh_tokens(
                    &deps.http,
                    &deps.clock,
                    &meta,
                    &client_id,
                    None, // public client — no confidential secret to send
                    &refresh,
                )
                .await
                {
                    Ok(t) => {
                        oauth::save_tokens_with_telemetry(
                            &deps.storage,
                            &deps.clock,
                            &key,
                            &t,
                            Some(&telemetry_ctx),
                        )
                        .await?;
                        oauth::emit_oauth_refresh_success(&telemetry_ctx);
                        t
                    }
                    // Refresh token rejected → fall back to a fresh flow.
                    Err(oauth::OAuthError::RefreshRejected(_)) => {
                        oauth::emit_oauth_refresh_failure(&telemetry_ctx, "invalid_grant");
                        self.run_interactive_oauth(
                            config,
                            oauth_cfg,
                            &key,
                            deps,
                            None,
                            resource_metadata_url,
                        )
                        .await?
                    }
                    Err(e) => {
                        oauth::emit_oauth_refresh_failure(
                            &telemetry_ctx,
                            oauth_refresh_failure_reason(&e),
                        );
                        return Err(e.into());
                    }
                }
            }
            None => {
                self.run_interactive_oauth(
                    config,
                    oauth_cfg,
                    &key,
                    deps,
                    None,
                    resource_metadata_url,
                )
                .await?
            }
        };

        Ok((
            inject_bearer(&config.spec, token.access_token.expose_secret()),
            GrantProvenance::from_tokens(&token),
        ))
    }

    /// Step-up re-auth after a 403 `insufficient_scope`: persist the required
    /// `scope` onto the stored entry (auth.ts `markStepUpPending`/`stepUpScope`,
    /// 1896), then run a fresh interactive flow requesting that elevated scope
    /// and return the spec with the new Bearer. A refresh CANNOT elevate scope
    /// (RFC 6749 §6), so this always drives the PKCE flow. `resource_metadata_url`
    /// (§24c) is the `resource_metadata` param parsed from the SAME 403
    /// challenge that carried the elevated `scope`, when present (oracle
    /// `_stepUpAuthorize`: `if(e.resourceMetadataUrl)this._resourceMetadataUrl=
    /// e.resourceMetadataUrl`).
    async fn step_up_oauth_spec(
        &self,
        config: &McpServerConfig,
        scope: &str,
        resource_metadata_url: Option<&str>,
    ) -> Result<(McpTransportSpec, Option<GrantProvenance>), McpError> {
        let deps = self
            .oauth
            .as_ref()
            .ok_or_else(|| McpError::OAuth("oauth seam not wired".into()))?;
        let oauth_cfg = spec_oauth(&config.spec)
            .ok_or_else(|| McpError::OAuth("server has no oauth config".into()))?;
        let key = oauth::server_key(&config.name, &config.spec);
        let telemetry_ctx = oauth::McpOAuthTelemetryContext::for_server(&config.name, &config.spec);

        // §26b delta 3, second arm: an XAA-flagged server's 403 must stay on
        // the XAA path exactly as its 401 does (`reauth_oauth_spec` above).
        // Oracle `pEr` — the ONE entry point to the consent flow — opens with
        // `if(t.oauth?.xaa){ ...await gt(...); return }` (@182200767), so no
        // step-up, cached `stepUpScope`, or elevated-scope request can ever
        // reach `redirectToAuthorization` for an XAA server. Without this
        // guard the 403 branch WINS the race (it is evaluated before the 401
        // branch in `connect_locked_inner`) and binds a loopback listener +
        // opens a browser on an enterprise deployment that has no interactive
        // consent surface at all. The scope persist below is skipped for the
        // same reason: the oracle only writes `stepUpScope` from inside
        // `redirectToAuthorization`, which XAA never reaches.
        if oauth_cfg.xaa == Some(true) {
            let token = self
                .resolve_xaa_token_inner(config, &key, deps, true, resource_metadata_url)
                .await?;
            return Ok((
                inject_bearer(&config.spec, token.access_token.expose_secret()),
                GrantProvenance::from_tokens(&token),
            ));
        }

        // Persist the elevated scope so it survives even if the interactive flow
        // is interrupted and resumed later (auth.ts caches it on the stored
        // entry). Best-effort: a storage failure must not block the step-up.
        if let Ok(Some(mut stored)) = oauth::load_tokens(&deps.storage, &key).await {
            stored.step_up_scope = Some(scope.to_string());
            let _ = oauth::store_tokens_with_telemetry(
                &deps.storage,
                &deps.clock,
                &key,
                &stored,
                Some(&telemetry_ctx),
            )
            .await;
        }

        let token = self
            .run_interactive_oauth(
                config,
                oauth_cfg,
                &key,
                deps,
                Some(scope),
                resource_metadata_url,
            )
            .await?;
        Ok((
            inject_bearer(&config.spec, token.access_token.expose_secret()),
            GrantProvenance::from_tokens(&token),
        ))
    }

    /// Resolve an access token for an XAA-flagged server (auth.ts
    /// `performMCPXaaAuth`, 664-845). Thin wrapper over
    /// [`Self::resolve_xaa_token_inner`] for the per-connect (non-401) resolve
    /// path, which may reuse a cached token.
    async fn resolve_xaa_token(
        &self,
        config: &McpServerConfig,
        key: &str,
        deps: &OAuthDeps,
    ) -> Result<oauth::Tokens, McpError> {
        self.resolve_xaa_token_inner(config, key, deps, false, None)
            .await
    }

    /// Core of the XAA resolve chain (oracle `tokens()`, @182213696), shared by
    /// the per-connect resolve ([`Self::resolve_xaa_token`], `force_fresh:
    /// false` — may reuse a cached token) and the post-401 re-auth
    /// ([`Self::reauth_oauth_spec`]'s xaa arm, `force_fresh: true` — the
    /// server just rejected whatever is cached, so the cache-hit branches
    /// below are skipped and a refresh/exchange is always attempted).
    /// `resource_metadata_url` (§24c) is the live 401 challenge's
    /// `resource_metadata` param, when any (`None` from the per-connect
    /// wrapper, which has no live challenge to read).
    ///
    /// Gating (auth.ts:871-876): `LINGXI_ENABLE_XAA` must be truthy or this
    /// hard-fails with actionable copy. Single-flight (§26b delta 2, oracle
    /// `_refreshInProgress`): callers for the SAME `key` serialize on
    /// [`Self::xaa_refresh_lock`], so at most one refresh/exchange runs at a
    /// time; a resolver that has to wait re-reads storage once it acquires the
    /// lock and reuses whatever the winner just persisted instead of
    /// re-exchanging.
    ///
    /// §26b delta 4: once a refresh token is on file, it ALWAYS takes the
    /// ordinary refresh route (`oauth::refresh_tokens`, whose confidential-
    /// client auth method is chosen from the AS's advertised
    /// `token_endpoint_auth_methods_supported`) — the full IdP+AS exchange
    /// ([`crate::xaa::perform_cross_app_access`]) is reserved for "no refresh
    /// token stored" (never had one, or the AS just rejected it, which falls
    /// through rather than opening an interactive flow — XAA is never
    /// interactive). §26b delta 1: absent a refresh token, the exchange is
    /// silent-triggered only when the access token is missing or expires
    /// within 300s (oracle `!n?.refreshToken && (!n?.accessToken ||
    /// (n.expiresAt-Date.now())/1000<=300)`); otherwise the cached access
    /// token is reused as-is. That same 300s window ALSO bounds reuse on the
    /// refresh-token arm (oracle `tokens()`'s `r<=300&&n.refreshToken`
    /// proactive refresh, which runs for every server after the XAA block).
    /// `force_fresh` skips both cache-hit checks.
    async fn resolve_xaa_token_inner(
        &self,
        config: &McpServerConfig,
        key: &str,
        deps: &OAuthDeps,
        force_fresh: bool,
        resource_metadata_url: Option<&str>,
    ) -> Result<oauth::Tokens, McpError> {
        // Gate on the enable flag (mirror of CLAUDE_CODE_ENABLE_XAA).
        if !platform_api::env::is_env_truthy(std::env::var("LINGXI_ENABLE_XAA").ok().as_deref()) {
            return Err(McpError::OAuth(format!(
                "XAA is not enabled (set LINGXI_ENABLE_XAA=1). Remove 'xaa' from \
                 server '{}' to use the standard consent flow.",
                config.name
            )));
        }

        // Single-flight: serialize concurrent resolves for this server key.
        let lock = self.xaa_refresh_lock(key);
        let _single_flight = lock.lock().await;

        let stored = oauth::load_tokens(&deps.storage, key).await?;

        match stored {
            Some(s) => {
                if let Some(refresh) = s.refresh_token.clone() {
                    // Delta 4: a refresh token on file takes the ordinary
                    // refresh route — never the full exchange below.
                    //
                    // Reuse is bounded by the SAME 300s proactive window the
                    // no-refresh-token arm below uses: oracle `tokens()`
                    // (@182213696) runs `if(r!=null&&r<=300&&n.refreshToken
                    // &&!d){...refreshAuthorization(n.refreshToken)...}`
                    // AFTER the XAA block, so a stored refresh token does not
                    // exempt a token from proactive refresh — it is what
                    // makes proactive refresh possible. Reusing until hard
                    // expiry instead hands the transport a token seconds from
                    // death, buying an avoidable 401 + reauth round-trip (or
                    // a connect failure if it lapses mid-handshake).
                    let expiring_soon = s
                        .expires_at()
                        .duration_since(deps.clock.now())
                        .map(|remaining| remaining <= Duration::from_secs(300))
                        .unwrap_or(true);
                    if !force_fresh && !expiring_soon {
                        return Ok(s.into_tokens());
                    }
                    let meta = oauth::discover_auth_server_metadata(
                        &deps.http,
                        spec_url(&config.spec),
                        None,
                        resource_metadata_url,
                    )
                    .await?;
                    let client_id = s.client_id.clone().unwrap_or_default();
                    let client_secret = s.client_secret.clone();
                    match oauth::refresh_tokens(
                        &deps.http,
                        &deps.clock,
                        &meta,
                        &client_id,
                        client_secret.as_deref(),
                        &refresh,
                    )
                    .await
                    {
                        Ok(refreshed) => {
                            let stored_new = oauth::StoredTokens {
                                access_token: refreshed.access_token.expose_secret().clone(),
                                refresh_token: refreshed
                                    .refresh_token
                                    .as_ref()
                                    .map(|t| t.expose_secret().clone()),
                                expires_at_unix: refreshed
                                    .expires_at
                                    .duration_since(SystemTime::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_secs(),
                                client_id: Some(client_id),
                                client_secret,
                                step_up_scope: None,
                            };
                            oauth::store_tokens(&deps.storage, &deps.clock, key, &stored_new)
                                .await
                                .map_err(McpError::from)?;
                            return Ok(stored_new.into_tokens());
                        }
                        // The refresh token itself is dead — fall through to
                        // the silent IdP+AS exchange below rather than a
                        // fresh interactive flow (XAA is never interactive).
                        Err(oauth::OAuthError::RefreshRejected(_)) => {}
                        Err(e) => return Err(e.into()),
                    }
                } else if !force_fresh {
                    // Delta 1: no refresh token — reuse the cached access
                    // token outright unless it is missing or expiring within
                    // 300s (`Err` from `duration_since` means already past).
                    let expiring_soon = s
                        .expires_at()
                        .duration_since(deps.clock.now())
                        .map(|remaining| remaining <= Duration::from_secs(300))
                        .unwrap_or(true);
                    if !expiring_soon {
                        return Ok(s.into_tokens());
                    }
                    tracing::debug!(
                        server = %config.name,
                        "XAA: access_token expiring, attempting silent exchange"
                    );
                }
            }
            None => {
                tracing::debug!(
                    server = %config.name,
                    "XAA: no access_token yet, attempting silent exchange"
                );
            }
        }

        // The IdP-login + AS-secret surface is supplied by the host provider.
        // Absent it, XAA cannot proceed — a clearly-noted residual seam.
        let provider = deps.xaa_config.as_ref().ok_or_else(|| {
            McpError::OAuth(format!(
                "XAA: no IdP connection configured for server '{}'. The XAA \
                 IdP-login/secret config surface (getXaaIdpSettings / \
                 acquireIdpIdToken / mcpOAuthClientConfig) is not yet wired \
                 (residual).",
                config.name
            ))
        })?;
        let server_url = spec_url(&config.spec);
        // The oracle performs this read before entering the failure-telemetry
        // try/catch. A storage failure therefore aborts the flow rather than
        // being mislabeled as an IdP cache miss.
        let id_token_cache_hit = provider
            .peek_id_token_cache_hit(&config.name, server_url)
            .await?;
        let inputs = match provider.xaa_inputs(&config.name, server_url).await {
            Ok(Some(inputs)) => inputs,
            Ok(None) => {
                emit_oauth_flow_failure(&telemetry::tengu::mcp::OAuthFlowFailurePayload {
                    auth_method: telemetry::Verified::assert_safe("xaa".to_string()),
                    xaa_failure_stage: telemetry::Verified::assert_safe("idp_login".to_string()),
                    id_token_cache_hit,
                });
                return Err(McpError::OAuth(format!(
                    "XAA: server '{}' is not XAA-provisioned (no IdP/AS inputs).",
                    config.name
                )));
            }
            Err(error) => {
                emit_oauth_flow_failure(&telemetry::tengu::mcp::OAuthFlowFailurePayload {
                    auth_method: telemetry::Verified::assert_safe("xaa".to_string()),
                    xaa_failure_stage: telemetry::Verified::assert_safe(
                        xaa_provider_failure_stage(&error).to_string(),
                    ),
                    id_token_cache_hit,
                });
                return Err(error);
            }
        };

        let result = match crate::xaa::perform_cross_app_access(
            &deps.http,
            spec_url(&config.spec),
            &crate::xaa::XaaConfig {
                client_id: &inputs.client_id,
                client_secret: &inputs.client_secret,
                idp_client_id: &inputs.idp_client_id,
                idp_client_secret: inputs.idp_client_secret.as_deref(),
                idp_id_token: &inputs.idp_id_token,
                idp_token_endpoint: &inputs.idp_token_endpoint,
            },
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(server = %config.name, "XAA silent exchange failed: {e}");
                emit_oauth_flow_failure(&telemetry::tengu::mcp::OAuthFlowFailurePayload {
                    auth_method: telemetry::Verified::assert_safe("xaa".to_string()),
                    xaa_failure_stage: telemetry::Verified::assert_safe(
                        xaa_flow_failure_stage(&e).to_string(),
                    ),
                    id_token_cache_hit,
                });
                // 4xx token-exchange ⇒ the cached id_token was rejected; drop it
                // so the next resolve re-acquires (auth.ts:1840-1847
                // `clearIdpIdToken(idp.issuer)`). 5xx (IdP outage) keeps it.
                // Best-effort: a clear failure must not mask the exchange error.
                if e.should_clear_id_token() {
                    let _ = provider.clear_id_token().await;
                }
                return Err(e.into());
            }
        };

        // Persist: carry the AS confidential client_id/secret so refresh +
        // RFC-7009 revocation can authenticate the confidential client
        // (auth.ts:807-825 token-save). `expires_in` (when present) sets expiry.
        let expires_at = match result.tokens.expires_in {
            Some(secs) => deps.clock.now() + Duration::from_secs(secs),
            // No expiry advertised → treat as already-stale so the next connect
            // re-runs the exchange (XAA tokens are cheap to re-mint, silent).
            None => deps.clock.now(),
        };
        let stored = oauth::StoredTokens {
            access_token: result.tokens.access_token.clone(),
            refresh_token: result.tokens.refresh_token.clone(),
            expires_at_unix: expires_at
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            client_id: Some(inputs.client_id.clone()),
            client_secret: Some(inputs.client_secret.clone()),
            step_up_scope: None,
        };
        oauth::store_tokens(&deps.storage, &deps.clock, key, &stored)
            .await
            .map_err(McpError::from)?;
        emit_xaa_oauth_flow_success(&telemetry::tengu::mcp::OAuthXaaFlowSuccessPayload {
            auth_method: telemetry::Verified::assert_safe("xaa".to_string()),
            id_token_cache_hit,
        });

        Ok(stored.into_tokens())
    }

    /// Drive the full interactive OAuth flow and persist the resulting tokens.
    ///
    /// `scope_override` carries an elevated scope cached from a prior 403
    /// `insufficient_scope` step-up (auth.ts `cachedStepUpScope`); when set the
    /// authorize URL requests it instead of the advertised scope. On a
    /// successful grant the persisted `step_up_scope` is cleared (auth.ts:1705).
    /// `resource_metadata_url` (§24c) carries a `resource_metadata` challenge
    /// param from the live 401/403 that triggered this flow (`None` for the
    /// proactive, no-challenge callers — a fresh server with no stored token,
    /// or a stale-token silent refresh that hasn't hit the wire yet).
    async fn run_interactive_oauth(
        &self,
        config: &McpServerConfig,
        oauth_cfg: &platform_api::McpOAuthConfigDto,
        key: &str,
        deps: &OAuthDeps,
        scope_override: Option<&str>,
        resource_metadata_url: Option<&str>,
    ) -> Result<oauth::Tokens, McpError> {
        // Effective elevated scope: an explicit override (the 403 step-up path)
        // wins; otherwise honor any `step_up_scope` cached on the stored entry
        // from a previous 403 `insufficient_scope` (auth.ts:906-909
        // `cachedStepUpScope`). The stored blob is about to be overwritten by the
        // fresh grant, so we read it before driving the flow.
        let pinned_scope = oauth_cfg
            .scopes
            .as_deref()
            .map(str::trim)
            .filter(|scope| !scope.is_empty())
            .map(str::to_string);
        let cached_scope = match (pinned_scope, scope_override) {
            (Some(scope), _) => Some(scope),
            (None, Some(scope)) => Some(scope.to_string()),
            (None, None) => oauth::load_tokens(&deps.storage, key)
                .await
                .ok()
                .flatten()
                .and_then(|t| t.step_up_scope),
        };

        // Surface `AwaitingOAuth` while the user completes the browser flow.
        let callback_port = oauth_cfg.callback_port.unwrap_or(0);
        self.connections.write().await.insert(
            config.name.clone(),
            McpConnectionState::AwaitingOAuth {
                config: config.clone(),
                callback_port,
            },
        );
        let telemetry_ctx = oauth::McpOAuthTelemetryContext::for_server(&config.name, &config.spec);
        let tokens = oauth::perform_oauth_flow_for_reauth(
            &deps.http,
            &deps.clock,
            oauth_cfg,
            &config.name,
            spec_url(&config.spec),
            &deps.on_authorization_url,
            Some(&telemetry_ctx),
            cached_scope.as_deref(),
            resource_metadata_url,
        )
        .await?;
        // A fresh grant clears any pending step-up scope (auth.ts:1705): the new
        // tokens carry the elevated scope, so the cache must not linger.
        oauth::save_tokens_with_telemetry(
            &deps.storage,
            &deps.clock,
            key,
            &tokens,
            Some(&telemetry_ctx),
        )
        .await?;
        Ok(tokens)
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
    /// `u(name).type`): after disable it is [`platform_api::McpActionState::Disabled`];
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
    ) -> Result<Option<platform_api::McpActionState>, McpError> {
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
            return Ok(Some(platform_api::McpActionState::Disabled));
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
            conns
                .get(name)
                .map_or(platform_api::McpActionState::Failed, project_action_state)
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
    /// [`platform_api::McpServerInfo`] shape. Used by
    /// `OrchestratorHandle::list_mcp_servers` (M6-07) so `/mcp` can list
    /// the registry without exposing the internal state-machine enum.
    ///
    /// Returned list is sorted by `name` for stable display order.
    pub async fn snapshot(&self) -> Vec<platform_api::McpServerInfo> {
        let conns = self.connections.read().await;
        let mut out: Vec<platform_api::McpServerInfo> = conns
            .values()
            .map(|s| platform_api::McpServerInfo {
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
    pub async fn action_states(&self) -> Vec<(String, platform_api::McpActionState)> {
        let conns = self.connections.read().await;
        let mut out: Vec<(String, platform_api::McpActionState)> = conns
            .values()
            .map(|s| (s.name().to_string(), project_action_state(s)))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Failed servers with their sanitized error text, for the `ToolSearch`
    /// empty-result diagnostics note — a port of claude-code's `wZr(u())`
    /// (`failed_mcp_servers`). Every server whose action state projects to
    /// [`platform_api::McpActionState::Failed`] ("not connected") is included,
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
            .filter(|s| project_action_state(s) == platform_api::McpActionState::Failed)
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
    /// [`platform_api::McpStatus`] UI projection collapses these into `Disconnected`;
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
            .any(|s| project_action_state(s) == platform_api::McpActionState::Pending);
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
        let (cached, cache_only): (HashMap<String, Vec<platform_api::McpToolDto>>, Vec<String>) = {
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
        let push_tools = |tools: Vec<platform_api::McpToolDto>, out: &mut Vec<String>| {
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
        protocol::McpConnectionId,
        platform_api::McpPromptDto,
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
        connection_id: protocol::McpConnectionId,
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
    /// lives on the cached [`platform_api::McpToolDto::tool_name`], so the dispatch
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
mod connection_timeout_tests {
    //! MCPLIFE.4 — connect deadline parity with claude-code
    //! `getConnectionTimeoutMs()` (`parseInt(MCP_TIMEOUT) || 30000`).
    use super::mcp_connection_timeout;
    use std::time::Duration;

    // The only test in this crate that mutates MCP_TIMEOUT; the connect-path
    // tests don't assert the deadline, so the shared-env mutation is harmless.
    #[test]
    fn timeout_defaults_to_30s_and_honors_positive_env() {
        std::env::remove_var("MCP_TIMEOUT");
        assert_eq!(mcp_connection_timeout(), Duration::from_secs(30));
        std::env::set_var("MCP_TIMEOUT", "5000");
        assert_eq!(mcp_connection_timeout(), Duration::from_millis(5000));
        // parseInt(..) || 30000 — zero / non-numeric / empty fall back to 30s.
        std::env::set_var("MCP_TIMEOUT", "0");
        assert_eq!(mcp_connection_timeout(), Duration::from_secs(30));
        std::env::set_var("MCP_TIMEOUT", "not-a-number");
        assert_eq!(mcp_connection_timeout(), Duration::from_secs(30));
        std::env::remove_var("MCP_TIMEOUT");
    }
}

#[cfg(test)]
mod backoff_schedule_tests {
    //! MCP reconnect backoff schedule parity with claude-code
    //! `useManageMCPConnections.ts:372-461` (`MAX_RECONNECT_ATTEMPTS = 5`,
    //! `INITIAL_BACKOFF_MS = 1000`, `MAX_BACKOFF_MS = 30000`).
    use super::{backoff_for, post_attempt_backoff, INITIAL_BACKOFF, MAX_BACKOFF};
    use std::time::Duration;

    /// The ordered list of inter-attempt sleeps actually taken during a
    /// reconnect run of `max` attempts — derived from the same
    /// [`post_attempt_backoff`] decision the production loop uses.
    fn sleep_schedule(max: u32) -> Vec<Duration> {
        (1..=max)
            .filter_map(|attempt| post_attempt_backoff(attempt, max))
            .collect()
    }

    #[test]
    fn constants_match_claude_code() {
        assert_eq!(INITIAL_BACKOFF, Duration::from_millis(1000));
        assert_eq!(MAX_BACKOFF, Duration::from_secs(30));
    }

    #[test]
    fn no_leading_sleep_and_no_trailing_sleep_on_final_attempt() {
        let max = 5;
        // The final attempt never sleeps afterwards (no trailing sleep).
        assert_eq!(post_attempt_backoff(max, max), None);
        // Every non-final attempt sleeps. The first inter-attempt sleep happens
        // AFTER attempt 1 — the loop calls `connect` before any sleep, so
        // attempt 1 has no leading sleep.
        for attempt in 1..max {
            assert!(
                post_attempt_backoff(attempt, max).is_some(),
                "attempt {attempt} of {max} should be followed by a backoff",
            );
        }
        // Exactly `max - 1` sleeps occur across the whole run.
        assert_eq!(sleep_schedule(max).len(), (max - 1) as usize);
    }

    #[test]
    fn schedule_is_1_2_4_8_seconds_and_16s_is_never_slept() {
        // claude-code sleeps 1s, 2s, 4s, 8s between the 5 attempts. The 16s
        // value (and the 30s cap) is NEVER used — it would only ever be a
        // trailing sleep after the final attempt, which does not exist.
        assert_eq!(
            sleep_schedule(5),
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
            ],
        );
        assert!(
            !sleep_schedule(5).contains(&Duration::from_secs(16)),
            "the 16s backoff (attempt 5) must never be slept",
        );
    }

    #[test]
    fn attempt_fire_times_are_0_1_3_7_15_seconds() {
        // Cumulative offsets at which each of the 5 attempts fires. Attempt 1
        // at t = 0 proves there is no leading sleep.
        let mut fire_times = vec![Duration::ZERO];
        let mut acc = Duration::ZERO;
        for s in sleep_schedule(5) {
            acc += s;
            fire_times.push(acc);
        }
        assert_eq!(
            fire_times,
            vec![
                Duration::from_secs(0),
                Duration::from_secs(1),
                Duration::from_secs(3),
                Duration::from_secs(7),
                Duration::from_secs(15),
            ],
        );
    }

    #[test]
    fn backoff_for_is_exponential_and_capped_at_max() {
        assert_eq!(backoff_for(1), Duration::from_secs(1));
        assert_eq!(backoff_for(2), Duration::from_secs(2));
        assert_eq!(backoff_for(3), Duration::from_secs(4));
        assert_eq!(backoff_for(4), Duration::from_secs(8));
        assert_eq!(backoff_for(5), Duration::from_secs(16));
        // 1000 * 2^5 = 32s exceeds the 30s ceiling → clamped.
        assert_eq!(backoff_for(6), MAX_BACKOFF);
        // Large attempts saturate at the cap, never overflow.
        assert_eq!(backoff_for(100), MAX_BACKOFF);
    }
}

/// Borrow the `oauth` config block of an SSE/HTTP spec, if present. Other
/// transports (stdio, websocket, …) never carry OAuth → `None`.
fn spec_oauth(spec: &McpTransportSpec) -> Option<&platform_api::McpOAuthConfigDto> {
    match spec {
        McpTransportSpec::Sse { oauth, .. } | McpTransportSpec::Http { oauth, .. } => {
            oauth.as_ref()
        }
        _ => None,
    }
}

/// Endpoint URL of an SSE/HTTP spec (used as the OAuth `server_url` for
/// discovery and the `getServerKey` hash). Empty for non-remote specs.
fn spec_url(spec: &McpTransportSpec) -> &str {
    match spec {
        McpTransportSpec::Sse { url, .. } | McpTransportSpec::Http { url, .. } => url,
        _ => "",
    }
}

/// Clone `spec` with `Authorization: Bearer <token>` set in its headers map.
/// Only SSE/HTTP specs carry headers; other variants are returned unchanged.
fn inject_bearer(spec: &McpTransportSpec, access_token: &str) -> McpTransportSpec {
    let bearer = format!("Bearer {access_token}");
    match spec.clone() {
        McpTransportSpec::Sse {
            url,
            mut headers,
            headers_helper,
            oauth,
        } => {
            headers.insert("Authorization".into(), bearer);
            McpTransportSpec::Sse {
                url,
                headers,
                headers_helper,
                oauth,
            }
        }
        McpTransportSpec::Http {
            url,
            mut headers,
            headers_helper,
            oauth,
        } => {
            headers.insert("Authorization".into(), bearer);
            McpTransportSpec::Http {
                url,
                headers,
                headers_helper,
                oauth,
            }
        }
        other => other,
    }
}

/// Faithful-core 401 detection. The error reaching here is always the result
/// of `connect_attempt` (transport `connect` + `initialize`): SSE's pre-flight
/// GET returns `McpError::HttpResponse` directly on a non-2xx status
/// (`platforms/common/src/mcp_sse.rs`), and Streamable HTTP's `initialize`
/// unwraps the same shape from a synthetic JSON-RPC error's structured
/// `data: {httpStatus, wwwAuthenticate}` (`handshake_error`,
/// `platforms/posix/src/mcp.rs` — mirrored on any platform that wires a real
/// HTTP transport). The substring match on `"401"` remains as a fallback for
/// any OTHER path that still flattens to a string (e.g. a raw connection
/// failure whose message happens to mention a status code) — it is not
/// expected to be the primary match for a real 401 any more.
fn error_is_401(e: &McpError) -> bool {
    matches!(e, McpError::HttpResponse { status: 401, .. })
        || matches!(
            e,
            McpError::Connection(m) | McpError::Handshake(m) if m.contains("401")
        )
}

pub(crate) fn error_is_auth_response(error: &McpError) -> bool {
    error_is_401(error)
        || matches!(error, McpError::HttpResponse { status: 403, .. })
        || matches!(
            error,
            McpError::Connection(message) | McpError::Handshake(message)
                if message.contains("403")
        )
}

/// Faithful-core 403 `insufficient_scope` step-up detection (auth.ts
/// `wrapFetchWithStepUpDetection`, 1354-1374). The primary path is now
/// structural: `connect_attempt`'s error carries a genuine `www_authenticate`
/// header value in `McpError::HttpResponse` (see `error_is_401`'s note — SSE's
/// pre-flight GET, and Streamable HTTP's `initialize` via `handshake_error`),
/// so `www_authenticate.contains("insufficient_scope")` and
/// `extract_scope_from_www_auth` run against the real header text. The
/// `Connection`/`Handshake` string arms remain as a substring-matched
/// fallback (`"403"` + `"insufficient_scope"`, extracting a `scope="…"`/
/// `scope=…` token per RFC 6750 §3 — the same shape as the SDK's
/// `extractFieldFromWwwAuth`) for any error shape that still flattens to a
/// string. Returns the elevated scope when present.
///
/// A tool-call-time 403 (post-connect, i.e. `McpClient::call_tool_with_progress`
/// rather than `connect_attempt`) is a SEPARATE path: `mcp/src/client.rs`'s
/// `mcp_client_error_from_rpc` already reconstructs a structured
/// `McpClientError::HttpResponse` from the same
/// `MCP_HTTP_STATUS=…;WWW_AUTHENTICATE=…` marker, consumed by
/// `call_tool_with_auth_retry`'s `is_auth_response()` check — this function
/// is never called on that path, so it is out of scope here.
fn error_is_403_insufficient_scope(e: &McpError) -> Option<String> {
    if let McpError::HttpResponse {
        status: 403,
        www_authenticate: Some(value),
    } = e
    {
        return value
            .contains("insufficient_scope")
            .then(|| extract_scope_from_www_auth(value))
            .flatten();
    }
    let (McpError::Connection(msg) | McpError::Handshake(msg)) = e else {
        return None;
    };
    if !(msg.contains("403") && msg.contains("insufficient_scope")) {
        return None;
    }
    extract_scope_from_www_auth(msg)
}

/// Extract a `resource_metadata` challenge param (RFC 9728) from a connect
/// failure's `WWW-Authenticate` header, for both the 401-reauth and 403
/// step-up call sites (§24c: oracle `Be`/`H0e`, threaded into `discover_
/// auth_server_metadata` so a server that publishes its Protected Resource
/// Metadata somewhere other than the well-known guess still resolves).
/// Structural only: `McpError::HttpResponse` (the shape `connect_attempt`'s
/// error now carries — see `error_is_401`'s note) is the sole source; the
/// string-flattened `Connection`/`Handshake` fallback shapes elsewhere in this
/// file don't carry a real header to reparse, so they yield `None` here and
/// discovery falls back to its well-known guess, exactly as before this
/// finding.
fn error_resource_metadata_url(e: &McpError) -> Option<String> {
    let McpError::HttpResponse {
        www_authenticate: Some(value),
        ..
    } = e
    else {
        return None;
    };
    oauth::parse_www_authenticate_challenge(value).resource_metadata_url
}

/// Hand-rolled equivalent of `wwwAuth.match(/scope=(?:"([^"]+)"|([^\s,]+))/)`
/// (auth.ts:1365). Finds the first `scope=` and returns its value, honoring an
/// optional double-quoted form; an unquoted value runs to the first whitespace
/// or comma. Avoids a `regex` dependency.
fn extract_scope_from_www_auth(s: &str) -> Option<String> {
    let idx = s.find("scope=")?;
    let rest = &s[idx + "scope=".len()..];
    if let Some(after_quote) = rest.strip_prefix('"') {
        // Quoted: up to the next `"`.
        let end = after_quote.find('"')?;
        let scope = &after_quote[..end];
        return (!scope.is_empty()).then(|| scope.to_string());
    }
    // Unquoted: up to the first whitespace or comma.
    let end = rest
        .find(|c: char| c.is_whitespace() || c == ',')
        .unwrap_or(rest.len());
    let scope = &rest[..end];
    (!scope.is_empty()).then(|| scope.to_string())
}

/// Test-only re-exports of internal OAuth-error classifiers, so integration
/// tests (`tests/oauth_flow_test.rs`) can unit-check the step-up detection
/// without making the helpers part of the public API.
#[doc(hidden)]
pub mod test_support {
    use platform_api::McpError;

    /// See [`super::error_is_403_insufficient_scope`].
    #[must_use]
    pub fn error_is_403_insufficient_scope(e: &McpError) -> Option<String> {
        super::error_is_403_insufficient_scope(e)
    }
}

/// Sanitize a model-visible diagnostic string (an MCP server name or failure
/// message) — a 1:1 port of claude-code's `xLt`, used to build the `ToolSearch`
/// empty-result failed-server note. Steps mirror claude exactly:
///
/// 1. NFKC-normalize.
/// 2. Replace control (`\p{Cc}`) / format (`\p{Cf}`) characters and
///    U+2028/U+2029 with a space (claude's `qU`).
/// 3. Replace angle brackets, `"`, `;` and a set of fancy quote / bracket
///    characters with a space.
/// 4. Collapse runs of whitespace to a single space and trim.
/// 5. Truncate to 200 characters, appending `…` (U+2026) when it was longer.
///
/// Steps 2–4 all map their targets to a space and then collapse, so a single
/// pass that treats every stripped-or-whitespace character as a collapsing
/// space produces the same result.
fn sanitize_diagnostic(input: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    /// claude's `RJu`.
    const LIMIT: usize = 200;

    // The explicit character class claude replaces with a space (step 3).
    fn is_stripped(c: char) -> bool {
        matches!(
            c,
            '<' | '>'
                | '"'
                | ';'
                | '\u{2018}'
                | '\u{2019}'
                | '\u{201A}'
                | '\u{201C}'
                | '\u{201D}'
                | '\u{201E}'
                | '\u{00AB}'
                | '\u{00BB}'
                | '\u{2039}'
                | '\u{203A}'
                | '\u{2329}'
                | '\u{232A}'
                | '\u{27E8}'
                | '\u{27E9}'
                | '\u{27EA}'
                | '\u{27EB}'
                | '\u{3008}'
                | '\u{3009}'
                | '\u{300A}'
                | '\u{300B}'
        )
    }

    let normalized: String = input.nfkc().collect();
    let mut collapsed = String::with_capacity(normalized.len());
    let mut pending_space = false;
    for c in normalized.chars() {
        // Step 2 (`qU`): control / format chars + U+2028/U+2029, step 3's
        // explicit class, and any other whitespace (step 4's `\s+`) all become
        // collapsing spaces.
        let is_control_or_format =
            c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') || is_format_char(c);
        if is_control_or_format || is_stripped(c) || c.is_whitespace() {
            pending_space = true;
            continue;
        }
        if pending_space && !collapsed.is_empty() {
            collapsed.push(' ');
        }
        pending_space = false;
        collapsed.push(c);
    }
    // `collapsed` already has no leading/interior double spaces or trailing
    // space (pending_space is dropped at end), i.e. it is already trimmed.
    if collapsed.chars().count() > LIMIT {
        let head: String = collapsed.chars().take(LIMIT).collect();
        format!("{head}\u{2026}")
    } else {
        collapsed
    }
}

/// Whether `c` is a Unicode format character (general category `Cf`) — the
/// portion of claude's `qU` (`\p{Cf}`) not covered by [`char::is_control`]
/// (which is `Cc`). Enumerates the stable `Cf` code-point blocks.
fn is_format_char(c: char) -> bool {
    let cp = c as u32;
    matches!(
        cp,
        0x00AD              // SOFT HYPHEN
        | 0x0600..=0x0605   // Arabic number signs
        | 0x061C            // Arabic Letter Mark
        | 0x06DD            // Arabic End of Ayah
        | 0x070F            // Syriac Abbreviation Mark
        | 0x0890..=0x0891   // Arabic pound / piastre marks
        | 0x08E2            // Arabic disputed end of ayah
        | 0x180E            // Mongolian vowel separator
        | 0x200B..=0x200F   // zero-width + LTR/RTL marks
        | 0x202A..=0x202E   // directional formatting
        | 0x2060..=0x2064   // word joiner + invisible operators
        | 0x2066..=0x206F   // directionality + deprecated
        | 0xFEFF            // ZERO WIDTH NO-BREAK SPACE / BOM
        | 0xFFF9..=0xFFFB   // interlinear annotation
        | 0x110BD           // Kaithi number sign
        | 0x110CD           // Kaithi number sign above
        | 0x13430..=0x1343F // Egyptian Hieroglyph format controls
        | 0x1BCA0..=0x1BCA3 // Shorthand format controls
        | 0x1D173..=0x1D17A // Musical symbol begin/end
        | 0xE0001           // Language tag
        | 0xE0020..=0xE007F // Tags block
    )
}

/// §20b — `tengu_mcp_server_config_invalid`: a server's config failed the
/// loader-time or connect-time URL/shape re-validation. Oracle call site:
/// `s("tengu_mcp_server_config_invalid",{transportType:c(t.type??"stdio"),
/// field:w("url"),source:w(t.configError?"loader":"connect")})` — `field` is
/// always the literal `"url"`, the sole re-validation target either gate
/// checks (see [`McpServerConfig::config_error`] /
/// [`McpServerConfig::connect_time_url_error`]'s docs for the two gates this
/// fires from).
/// §20b — build `tengu_mcp_tools_listed`'s payload. Pure: takes the already
/// resolved/filtered tool list and elapsed duration rather than reaching
/// into `self`/`conn`, so the field-mapping (`tool_count`/`always_load_count`
/// off the FINAL post-§20a-filter list, not the raw transport response) is
/// unit-testable without standing up a mock transport.
///
/// `discovery_source` is unconditionally `"live"` here — this is the
/// CONNECT path (a fresh dial), never the cached-row-adoption path the
/// oracle's `discoverySource` also covers (§18's deferred
/// `cached-row adopt subscriber threw` item; this port has no cached-row
/// adoption at all yet).
/// Oracle `yn`'s FIRST statement (@182316780):
/// `if(u.length===0&&r==="live")s("tengu_mcp_degraded",{reason:w("connected_zero_tools"),…})`.
///
/// Pure and separate from the `connect` body so the three conditions are
/// unit-testable without standing up a transport:
///
/// * `u.length === 0` — `u` is the RAW `tools/list` response, so emptiness is
///   measured BEFORE the §20a schema filter runs. A server whose every tool
///   that filter dropped reports its drop reason, NOT zero-tools.
/// * `r === "live"` — always true on this path (a fresh dial); the oracle's
///   cached-row adoption path, which also feeds `yn`, does not exist here.
/// * `caps.tools` — with no tools capability the oracle never reaches `yn`,
///   so an empty list there is not a degraded signal (same gate as
///   `tengu_mcp_tools_listed`).
fn connected_zero_tools_fires(caps_tools: bool, raw_tool_count: usize) -> bool {
    caps_tools && raw_tool_count == 0
}

/// §11 — pure core of `McpRegistry::connect_locked_inner`'s MISS branch:
/// given the pre-dial [`crate::discovery_cache::Decision`], what `source`
/// string to emit on `tengu_mcp_discovery_source` (if anything). `None`
/// means do not emit at all — either a `Fresh`/`Stale` decision (a HIT is
/// handled entirely separately by `McpRegistry::serve_discovery_cache_hit`,
/// which emits its own `"cache_fresh"`/`"cache_stale"` — this function is
/// never even called for one), or a `Miss` reason the oracle's `Ko` gate
/// excludes (`Disabled`/`Transport`/`LiveConnection`/`SkillsCapable`/
/// `ChannelCapable`).
fn discovery_source_emission(decision: &crate::discovery_cache::Decision) -> Option<&'static str> {
    match decision {
        crate::discovery_cache::Decision::Miss { reason }
            if crate::discovery_cache::miss_emits_discovery_source_telemetry(*reason) =>
        {
            Some(crate::discovery_cache::miss_telemetry_value(*reason))
        }
        _ => None,
    }
}

fn tools_listed_payload(
    transport_kind: &str,
    elapsed: std::time::Duration,
    tools: &[platform_api::McpToolDto],
    server_name: &str,
) -> telemetry::tengu::mcp::ToolsListedPayload {
    use telemetry::pii::Verified;
    telemetry::tengu::mcp::ToolsListedPayload {
        transport_type: Verified::assert_safe(transport_kind.to_string()),
        list_duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        tool_count: u32::try_from(tools.len()).unwrap_or(u32::MAX),
        always_load_count: u32::try_from(
            tools.iter().filter(|t| t.always_load == Some(true)).count(),
        )
        .unwrap_or(u32::MAX),
        discovery_source: Verified::assert_safe("live".to_string()),
        // Oracle: `mcpServerName:EA(ln(e.name),HT(e.name,e.config))` — `EA`
        // returns `undefined` (the spread DROPS the key) unless the
        // first-party gate holds. Emitting the raw name unconditionally, as
        // an earlier revision did, made a user's private server name an
        // analytics dimension on every connect. See
        // `telemetry::tengu::mcp::server_name_gate`.
        mcp_server_name: telemetry::tengu::mcp::server_name_gate(transport_kind)
            .then(|| Verified::assert_safe(server_name.to_string())),
    }
}

fn emit_server_config_invalid(
    config: &McpServerConfig,
    source: telemetry::tengu::mcp::ConfigInvalidSource,
) {
    telemetry::emit_mcp_server_config_invalid(&server_config_invalid_payload(config, source));
}

/// Pure payload-building half of [`emit_server_config_invalid`] — split out
/// so the loader-vs-connect classification is unit-testable directly,
/// without a tracing-capture race (see `degraded_payloads_for_server`'s doc
/// for why that race is real in this shared test binary).
fn server_config_invalid_payload(
    config: &McpServerConfig,
    source: telemetry::tengu::mcp::ConfigInvalidSource,
) -> telemetry::tengu::mcp::ServerConfigInvalidPayload {
    use telemetry::pii::Verified;
    telemetry::tengu::mcp::ServerConfigInvalidPayload {
        transport_type: Verified::assert_safe(config.spec.kind().to_string()),
        field: Verified::assert_safe("url".to_string()),
        source,
    }
}

fn config_scope_wire(scope: ConfigScope) -> &'static str {
    match scope {
        ConfigScope::Settings(protocol::SettingsScope::Local) => "local",
        ConfigScope::Settings(protocol::SettingsScope::User) => "user",
        ConfigScope::Settings(protocol::SettingsScope::Project) => "project",
        ConfigScope::Dynamic => "dynamic",
        ConfigScope::Enterprise => "enterprise",
        ConfigScope::ClaudeAi => "claudeai",
        ConfigScope::Settings(protocol::SettingsScope::Managed) => "managed",
        ConfigScope::Agent => "agent",
    }
}

fn negotiation_mode_wire(
    negotiation_mode: crate::protocol_negotiation::NegotiationMode,
) -> &'static str {
    match negotiation_mode {
        crate::protocol_negotiation::NegotiationMode::Legacy => "legacy",
        crate::protocol_negotiation::NegotiationMode::Auto { .. } => "auto",
    }
}

fn protocol_era_wire(era: platform_api::McpProtocolEra) -> &'static str {
    match era {
        platform_api::McpProtocolEra::Legacy => "legacy",
        platform_api::McpProtocolEra::Modern => "modern",
    }
}

fn is_plugin_mcp_config(config: &McpServerConfig) -> bool {
    matches!(
        config.metadata.agent_source,
        Some(crate::connection::McpAgentSource::Plugin)
    )
}

fn server_connection_succeeded_payload(
    config: &McpServerConfig,
    connection_duration_ms: u64,
    negotiation_mode: crate::protocol_negotiation::NegotiationMode,
    negotiated: &platform_api::McpNegotiatedProtocol,
) -> telemetry::tengu::mcp::ServerConnectionSucceededPayload {
    use telemetry::pii::Verified;
    telemetry::tengu::mcp::ServerConnectionSucceededPayload {
        connection_duration_ms,
        transport_type: Verified::assert_safe(config.spec.kind().to_string()),
        scope: Verified::assert_safe(config_scope_wire(config.scope).to_string()),
        is_plugin: is_plugin_mcp_config(config),
        negotiation_mode: Some(Verified::assert_safe(
            negotiation_mode_wire(negotiation_mode).to_string(),
        )),
        protocol_era: Some(Verified::assert_safe(
            protocol_era_wire(negotiated.era).to_string(),
        )),
        negotiated_protocol_version: Some(Verified::assert_safe(negotiated.version.clone())),
    }
}

fn server_connection_failed_payload(
    config: &McpServerConfig,
    negotiation_mode: Option<crate::protocol_negotiation::NegotiationMode>,
    connection_duration_ms: Option<u64>,
    error_code: Option<&'static str>,
) -> telemetry::tengu::mcp::ServerConnectionFailedPayload {
    use telemetry::pii::Verified;
    telemetry::tengu::mcp::ServerConnectionFailedPayload {
        transport_type: Verified::assert_safe(config.spec.kind().to_string()),
        scope: Verified::assert_safe(config_scope_wire(config.scope).to_string()),
        is_plugin: is_plugin_mcp_config(config),
        connection_duration_ms,
        negotiation_mode: negotiation_mode
            .map(negotiation_mode_wire)
            .map(|mode| Verified::assert_safe(mode.to_string())),
        error_code: error_code.map(|code| Verified::assert_safe(code.to_string())),
    }
}

fn list_changed_payload(
    server_name: &str,
    kind: McpCatalogKind,
    cause: &'static str,
    previous_count: Option<usize>,
    new_count: Option<usize>,
) -> telemetry::tengu::mcp::ListChangedPayload {
    use telemetry::pii::Verified;
    telemetry::tengu::mcp::ListChangedPayload {
        kind: match kind {
            McpCatalogKind::Tools => telemetry::tengu::mcp::ListChangedType::Tools,
            McpCatalogKind::Prompts => telemetry::tengu::mcp::ListChangedType::Prompts,
            McpCatalogKind::Resources => telemetry::tengu::mcp::ListChangedType::Resources,
        },
        mcp_server_key_hash: mcp_server_key_hash(server_name),
        cause: Verified::assert_safe(cause.to_string()),
        previous_count: previous_count
            .map(|count| u32::try_from(count).unwrap_or(u32::MAX))
            .filter(|_| kind == McpCatalogKind::Tools),
        new_count: new_count
            .map(|count| u32::try_from(count).unwrap_or(u32::MAX))
            .filter(|_| kind == McpCatalogKind::Tools),
    }
}

fn modern_listen_request_params(
    version: &str,
    notifications: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "_meta": crate::client::modern_meta(version),
        "notifications": notifications,
    })
}

fn modern_listen_notifications_filter(
    capabilities: &ServerCapabilitiesDto,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut notifications = serde_json::Map::new();
    if capabilities.tools {
        notifications.insert(
            "toolsListChanged".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    if capabilities.prompts {
        notifications.insert(
            "promptsListChanged".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    if capabilities.resources {
        notifications.insert(
            "resourcesListChanged".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    (!notifications.is_empty()).then_some(notifications)
}

fn notification_matches_subscription(
    notification: &jsonrpc::Notification,
    subscription_id: &jsonrpc::Id,
) -> bool {
    let Some(params) = notification.params.as_ref() else {
        return false;
    };
    let Some(meta) = params.get("_meta").and_then(serde_json::Value::as_object) else {
        return false;
    };
    let Some(observed) = meta.get("io.modelcontextprotocol/subscriptionId") else {
        return false;
    };
    match subscription_id {
        jsonrpc::Id::Number(expected) => observed.as_i64() == Some(*expected),
        jsonrpc::Id::String(expected) => observed.as_str() == Some(expected),
    }
}

fn listener_reopen_park_jitter() -> f64 {
    #[cfg(test)]
    if let Some(jitter) = test_listener_reopen_park_jitter() {
        return jitter;
    }
    rand::rng().random_range(0.8_f64..=1.2_f64)
}

fn resource_templates_fetched_payload(
    templates: &[platform_api::McpResourceTemplateDto],
) -> telemetry::tengu::mcp::ResourceTemplatesFetchedPayload {
    telemetry::tengu::mcp::ResourceTemplatesFetchedPayload {
        template_count: u32::try_from(templates.len()).unwrap_or(u32::MAX),
    }
}

fn mcp_server_key_hash(server_name: &str) -> telemetry::pii::Verified {
    oauth::telemetry_server_key_hash_for_key(server_name)
}

fn emit_oauth_flow_failure(payload: &telemetry::tengu::mcp::OAuthFlowFailurePayload) {
    tracing::info!(
        event = telemetry::tengu::mcp::OAUTH_FLOW_FAILURE,
        authMethod = payload.auth_method.as_str(),
        xaaFailureStage = payload.xaa_failure_stage.as_str(),
        idTokenCacheHit = payload.id_token_cache_hit,
    );
    let attrs = std::iter::IntoIterator::into_iter([
        (
            "authMethod".to_string(),
            telemetry::otel::AttrValue::from(payload.auth_method.as_str().to_string()),
        ),
        (
            "xaaFailureStage".to_string(),
            telemetry::otel::AttrValue::from(payload.xaa_failure_stage.as_str().to_string()),
        ),
        (
            "idTokenCacheHit".to_string(),
            telemetry::otel::AttrValue::from(payload.id_token_cache_hit),
        ),
    ])
    .collect();
    telemetry::otel::emit_named_log_event(telemetry::tengu::mcp::OAUTH_FLOW_FAILURE, &attrs);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::OAUTH_FLOW_FAILURE,
        serde_json::to_value(payload).expect("serialize test oauth_flow_failure payload"),
    );
}

fn emit_xaa_oauth_flow_success(payload: &telemetry::tengu::mcp::OAuthXaaFlowSuccessPayload) {
    tracing::info!(
        event = telemetry::tengu::mcp::OAUTH_FLOW_SUCCESS,
        authMethod = payload.auth_method.as_str(),
        idTokenCacheHit = payload.id_token_cache_hit,
    );
    let attrs = std::iter::IntoIterator::into_iter([
        (
            "authMethod".to_string(),
            telemetry::otel::AttrValue::from(payload.auth_method.as_str().to_string()),
        ),
        (
            "idTokenCacheHit".to_string(),
            telemetry::otel::AttrValue::from(payload.id_token_cache_hit),
        ),
    ])
    .collect();
    telemetry::otel::emit_named_log_event(telemetry::tengu::mcp::OAUTH_FLOW_SUCCESS, &attrs);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::OAUTH_FLOW_SUCCESS,
        serde_json::to_value(payload).expect("serialize test XAA oauth_flow_success payload"),
    );
}

fn telemetry_mcp_server_base_url(
    spec: &platform_api::McpTransportSpec,
) -> Option<telemetry::Verified> {
    let mut url = url::Url::parse(spec_url(spec)).ok()?;
    url.set_query(None);
    url.set_fragment(None);
    url.set_username("").ok()?;
    url.set_password(None).ok()?;
    let normalized = url.to_string();
    let normalized = normalized.strip_suffix('/').unwrap_or(normalized.as_str());
    // Oracle `gg(IAe(config))` hashes the credential/query/fragment-free URL
    // with SHA-256 and keeps the first 12 hex digits. The misleading
    // `mcpServerBaseUrl` field name must not cause the normalized URL itself to
    // leave the process.
    Some(oauth::telemetry_server_key_hash_for_key(normalized))
}

fn emit_session_expired(payload: &telemetry::tengu::mcp::SessionExpiredPayload) {
    match payload.error_code.as_ref() {
        Some(error_code) => tracing::info!(
            event = telemetry::tengu::mcp::SESSION_EXPIRED,
            errorCode = error_code.as_str(),
            transportType = payload.transport_type.as_str(),
            mcpServerKeyHash = payload.mcp_server_key_hash.as_str(),
            mcpServerBaseUrl = payload
                .mcp_server_base_url
                .as_ref()
                .map(telemetry::Verified::as_str),
        ),
        None => tracing::info!(
            event = telemetry::tengu::mcp::SESSION_EXPIRED,
            transportType = payload.transport_type.as_str(),
            mcpServerKeyHash = payload.mcp_server_key_hash.as_str(),
            mcpServerBaseUrl = payload
                .mcp_server_base_url
                .as_ref()
                .map(telemetry::Verified::as_str),
        ),
    }
    let mut attrs = std::collections::BTreeMap::from([
        (
            "transportType".to_string(),
            telemetry::otel::AttrValue::from(payload.transport_type.as_str().to_string()),
        ),
        (
            "mcpServerKeyHash".to_string(),
            telemetry::otel::AttrValue::from(payload.mcp_server_key_hash.as_str().to_string()),
        ),
    ]);
    if let Some(error_code) = payload.error_code.as_ref() {
        attrs.insert(
            "errorCode".to_string(),
            telemetry::otel::AttrValue::from(error_code.as_str().to_string()),
        );
    }
    if let Some(base_url) = payload.mcp_server_base_url.as_ref() {
        attrs.insert(
            "mcpServerBaseUrl".to_string(),
            telemetry::otel::AttrValue::from(base_url.as_str().to_string()),
        );
    }
    telemetry::otel::emit_named_log_event(telemetry::tengu::mcp::SESSION_EXPIRED, &attrs);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::SESSION_EXPIRED,
        serde_json::to_value(payload).expect("serialize test session_expired payload"),
    );
}

fn emit_server_needs_auth_for_config(config: &McpServerConfig, cause: Option<&str>) {
    let telemetry = oauth::McpOAuthTelemetryContext::for_server(&config.name, &config.spec);
    let payload = telemetry::tengu::mcp::ServerNeedsAuthPayload {
        transport_type: telemetry.transport_type,
        mcp_server_key_hash: telemetry.mcp_server_key_hash,
        cause: cause.map(|value| telemetry::Verified::assert_safe(value.to_string())),
    };
    telemetry::emit_mcp_server_needs_auth(&payload);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::SERVER_NEEDS_AUTH,
        serde_json::to_value(payload).unwrap(),
    );
}

fn emit_tool_call_auth_error_for_config(
    config: &McpServerConfig,
    error_code: &str,
    auth_error_kind: telemetry::tengu::mcp::ToolCallAuthErrorKind,
) {
    let telemetry = oauth::McpOAuthTelemetryContext::for_server(&config.name, &config.spec);
    let payload = telemetry::tengu::mcp::ToolCallAuthErrorPayload {
        error_code: telemetry::Verified::assert_safe(error_code.to_string()),
        transport_type: telemetry.transport_type,
        auth_error_kind,
        mcp_server_key_hash: telemetry.mcp_server_key_hash,
    };
    telemetry::emit_mcp_tool_call_auth_error(&payload);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::TOOL_CALL_AUTH_ERROR,
        serde_json::to_value(payload).unwrap(),
    );
}

fn oauth_refresh_failure_reason(error: &oauth::OAuthError) -> &'static str {
    match error {
        oauth::OAuthError::Discovery(_) => "metadata_discovery_failed",
        oauth::OAuthError::RefreshRejected(_) => "invalid_grant",
        oauth::OAuthError::Token(message) if message.contains("invalid_client") => "invalid_client",
        oauth::OAuthError::Token(message) if message.contains("unauthorized_client") => {
            "unauthorized_client"
        }
        oauth::OAuthError::Token(message) if message.contains("decode") => {
            "token_response_schema_rejected"
        }
        oauth::OAuthError::Token(_) => "request_failed",
        oauth::OAuthError::Callback(_) => "request_failed",
        oauth::OAuthError::Registration(_) => "request_failed",
    }
}

fn tool_call_auth_error_code(error: &crate::client::McpClientError) -> &'static str {
    match error {
        crate::client::McpClientError::HttpResponse { status, .. } if *status == 403 => "403",
        crate::client::McpClientError::HttpResponse { .. } => "401",
        _ => "401",
    }
}

fn xaa_flow_failure_stage(error: &crate::xaa::XaaError) -> &'static str {
    match error {
        crate::xaa::XaaError::TokenExchange { .. } => "token_exchange",
        crate::xaa::XaaError::JwtBearer(_) => "jwt_bearer",
        crate::xaa::XaaError::Prm(_)
        | crate::xaa::XaaError::NoAuthServer(_)
        | crate::xaa::XaaError::AsMetadata(_) => "discovery",
    }
}

fn xaa_provider_failure_stage(error: &McpError) -> &'static str {
    match error {
        McpError::OAuth(message)
            if message.starts_with("XAA IdP: OIDC discovery")
                || message.starts_with("XAA IdP: refusing non-HTTPS token endpoint") =>
        {
            "discovery"
        }
        _ => "idp_login",
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

fn emit_server_connection_succeeded(
    payload: &telemetry::tengu::mcp::ServerConnectionSucceededPayload,
) {
    telemetry::emit_mcp_server_connection_succeeded(payload);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::SERVER_CONNECTION_SUCCEEDED,
        serde_json::to_value(payload).expect("serialize test success payload"),
    );
}

fn emit_server_connection_failed(payload: &telemetry::tengu::mcp::ServerConnectionFailedPayload) {
    telemetry::emit_mcp_server_connection_failed(payload);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::SERVER_CONNECTION_FAILED,
        serde_json::to_value(payload).expect("serialize test failure payload"),
    );
}

fn emit_tools_listed(payload: &telemetry::tengu::mcp::ToolsListedPayload) {
    telemetry::emit_mcp_tools_listed(payload);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::TOOLS_LISTED,
        serde_json::to_value(payload).expect("serialize test tools_listed payload"),
    );
}

fn emit_degraded(payload: &telemetry::tengu::mcp::DegradedPayload) {
    telemetry::emit_mcp_degraded(payload);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::DEGRADED,
        serde_json::to_value(payload).expect("serialize test degraded payload"),
    );
}

fn emit_list_changed(payload: &telemetry::tengu::mcp::ListChangedPayload) {
    telemetry::emit_mcp_list_changed(payload);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::LIST_CHANGED,
        serde_json::to_value(payload).expect("serialize test list_changed payload"),
    );
}

fn emit_listen_reopen(payload: &telemetry::tengu::mcp::ListenReopenPayload) {
    telemetry::emit_mcp_listen_reopen(payload);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::LISTEN_REOPEN,
        serde_json::to_value(payload).expect("serialize test listen_reopen payload"),
    );
}

fn emit_resource_templates_fetched(
    payload: &telemetry::tengu::mcp::ResourceTemplatesFetchedPayload,
) {
    telemetry::emit_mcp_resource_templates_fetched(payload);
    #[cfg(test)]
    record_test_telemetry_event(
        telemetry::tengu::mcp::RESOURCE_TEMPLATES_FETCHED,
        serde_json::to_value(payload).expect("serialize test resource_templates payload"),
    );
}

fn listen_reopen_payload(
    server_name: &str,
    outcome: telemetry::tengu::mcp::ListenReopenOutcome,
    attempts: u32,
    trigger: telemetry::tengu::mcp::ListenReopenTrigger,
) -> telemetry::tengu::mcp::ListenReopenPayload {
    telemetry::tengu::mcp::ListenReopenPayload {
        mcp_server_key_hash: mcp_server_key_hash(server_name),
        outcome,
        attempts,
        trigger,
    }
}

fn notification_kind(method: &str) -> Option<McpCatalogKind> {
    match method {
        "notifications/tools/list_changed" => Some(McpCatalogKind::Tools),
        "notifications/prompts/list_changed" => Some(McpCatalogKind::Prompts),
        "notifications/resources/list_changed" => Some(McpCatalogKind::Resources),
        _ => None,
    }
}

fn forward_catalog_change(
    changes: &broadcast::Sender<McpCatalogChanged>,
    server_name: &str,
    connection_id: McpConnectionId,
    method: &str,
    telemetry_cause: Option<&'static str>,
) {
    let Some(kind) = notification_kind(method) else {
        return;
    };
    let _ = changes.send(McpCatalogChanged {
        server_name: server_name.to_string(),
        connection_id,
        retired_connection_id: None,
        kind,
        telemetry_cause,
    });
}

/// §20b — build the `tengu_mcp_degraded` payload for every NONZERO bucket in
/// one server's tallied tool-schema classification counts. Pure and
/// deterministic (no telemetry emission, no tracing) so the aggregation
/// logic — which count-field a reason maps to, and that every nonzero
/// bucket becomes exactly one payload — is unit-testable directly, without
/// racing a concurrently-running test's `tracing` subscriber over the
/// process-global callsite `Interest` cache (see the call site's doc for
/// why that race is real, not hypothetical).
fn degraded_payloads_for_server(
    counts: &std::collections::HashMap<telemetry::tengu::mcp::DegradedReason, u32>,
    transport_kind: &str,
    server_name: &str,
) -> Vec<telemetry::tengu::mcp::DegradedPayload> {
    use telemetry::pii::Verified;
    use telemetry::tengu::mcp::{DegradedPayload, DegradedReason};

    if counts.is_empty() {
        return Vec::new();
    }
    let transport_type = Verified::assert_safe(transport_kind.to_string());
    // Same gate as `tools_listed_payload` — the oracle spreads the SAME `P`
    // into every per-server `tengu_mcp_degraded`.
    let mcp_server_name = telemetry::tengu::mcp::server_name_gate(transport_kind)
        .then(|| Verified::assert_safe(server_name.to_string()));
    let mut out = Vec::with_capacity(counts.len());
    for (reason, count) in counts {
        let (normalized_count, skipped_count, kept_count) = match reason {
            // Oracle's `connected_zero_tools` payload is
            // `{reason,transportType,mcpServerName,..._}` — no count field.
            DegradedReason::ConnectedZeroTools
            | DegradedReason::ToolsListFailed
            | DegradedReason::ResourcesListFailed
            | DegradedReason::PromptsListFailed => (None, None, None),
            DegradedReason::ToolSchemaNormalized => (Some(*count), None, None),
            DegradedReason::ToolSchemaNormalizeGated
            | DegradedReason::ToolSchemaUnsupported
            | DegradedReason::ToolSchemaInvalid
            | DegradedReason::ToolPropertyKeyInvalid => (None, Some(*count), None),
            DegradedReason::ToolSchemaInvalidGated
            | DegradedReason::ToolPropertyKeyInvalidGated => (None, None, Some(*count)),
            // `SchemaValidatorUnavailable` is process-global (fired from
            // `tool_schema::meta_validator`, never tallied into this
            // per-server map) and the enum is `#[non_exhaustive]` — a future
            // oracle-confirmed sibling with no known count-field mapping
            // falls here too, skipped rather than guessed at.
            _ => continue,
        };
        out.push(DegradedPayload {
            reason: *reason,
            transport_type: Some(transport_type.clone()),
            normalized_count,
            skipped_count,
            kept_count,
            mcp_server_name: mcp_server_name.clone(),
        });
    }
    out
}

/// Whether a state's config is flagged `disabled` (mid-reconnect guard).
fn state_is_disabled(state: &McpConnectionState) -> bool {
    state.config().disabled
}

/// Project a [`McpConnectionState`] onto the fine-grained
/// [`platform_api::McpActionState`] used by the `/mcp reconnect|enable|disable`
/// action handler — a faithful mirror of claude-code's client `type`
/// discriminant. The `config.disabled` gate takes precedence (a disabled
/// server reports `"disabled"` regardless of its last live state), then:
/// `Connected`/`HealthChecking` → connected, `Connecting`/`Reconnecting` →
/// pending, `AwaitingOAuth` → needs-auth, everything else (`Failed`,
/// `Disconnected`, `Stopped`) → failed ("not connected").
fn project_action_state(state: &McpConnectionState) -> platform_api::McpActionState {
    use platform_api::McpActionState;
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
/// [`platform_api::McpStatus`] (M6-07).
fn project_status(state: &McpConnectionState) -> platform_api::McpStatus {
    use platform_api::McpStatus;
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
mod tests {
    //! Bridge (Batch 1) + FQN-rewrite / normalize-match (Batch 2) tests.
    //!
    //! A broader mock-transport lifecycle test lives in
    //! `test-harness/tests/mcp_lifecycle.rs`.
    use super::*;
    use crate::connection::{ConfigScope, McpServerConfig};
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures_util::StreamExt;
    use jsonrpc::{Connection, Mode};
    use platform_api::{
        ElicitRequestDto, ElicitResultDto, McpError, McpNotificationStream, McpPromptDto,
        McpRawConnection, McpResourceContentDto, McpResourceDto, McpToolDto, McpToolResultDto,
        McpTransport, McpTransportKind, McpTransportSpec, ServerCapabilitiesDto,
    };
    use protocol::McpConnectionId as ConnId;
    use serde_json::Value;
    use std::future::Future;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex as TestMutex;
    use std::task::{Context, Poll, Wake, Waker};
    use tokio::sync::{mpsc, Notify};

    /// Build a `Connection` over a fresh pair of `mpsc<Bytes>` channels (the
    /// `paired_connection` pattern from `client.rs:536`); the peer ends are
    /// dropped — these tests only assert client *presence*, not round-trips.
    fn paired_connection() -> Arc<Connection> {
        let (_peer_to_us_tx, peer_to_us_rx) = mpsc::channel::<Bytes>(8);
        let (us_to_peer_tx, _us_to_peer_rx) = mpsc::channel::<Bytes>(8);
        Arc::new(Connection::new_streams(
            peer_to_us_rx,
            us_to_peer_tx,
            Mode::Lines,
        ))
    }

    /// Paired connection retaining the mock-server ends for notification and
    /// request/response catalog-refresh tests.
    #[allow(clippy::type_complexity)]
    fn drivable_connection() -> (Arc<Connection>, mpsc::Sender<Bytes>, mpsc::Receiver<Bytes>) {
        let (peer_to_us_tx, peer_to_us_rx) = mpsc::channel::<Bytes>(8);
        let (us_to_peer_tx, us_to_peer_rx) = mpsc::channel::<Bytes>(8);
        (
            Arc::new(Connection::new_streams(
                peer_to_us_rx,
                us_to_peer_tx,
                Mode::Lines,
            )),
            peer_to_us_tx,
            us_to_peer_rx,
        )
    }

    /// Functional mock transport that ALSO bridges a paired in-memory
    /// `jsonrpc::Connection` through [`RawConnectionProvider`].
    ///
    /// On `connect` it mints a fresh id, stashes a paired connection under it,
    /// and returns canned tools (with the empty `<server>` token the real
    /// transport emits, so the registry's rewrite is exercised).
    struct BridgeMock {
        tools: Vec<McpToolDto>,
        resources: Vec<McpResourceDto>,
        prompts: Vec<McpPromptDto>,
        // §26a — canned `resources/templates/list` rows and the capability
        // presence bit that gates whether `connect` fetches them at all
        // (`initialize` reports `resources: true` only when this is set).
        resource_templates: Vec<platform_api::McpResourceTemplateDto>,
        resources_capability: AtomicBool,
        prompts_capability: AtomicBool,
        list_resource_templates_fails: AtomicBool,
        list_resources_fails: AtomicBool,
        list_prompts_fails: AtomicBool,
        drivable_calls: bool,
        modern_connect: bool,
        /// How many times `resources/templates/list` was actually issued. The
        /// parity claim is ZERO RPCs when the discovery cache is ineligible —
        /// an empty `resource_templates` field would also pass if we fetched
        /// and discarded, so the field alone cannot prove it.
        templates_calls: AtomicUsize,
        conns: TestMutex<HashMap<ConnId, Arc<Connection>>>,
        list_tools_fails: AtomicBool,
        block_list_tools: AtomicBool,
        list_tools_started: Notify,
        list_tools_release: Notify,
        disconnect_fails: AtomicBool,
        block_disconnect: AtomicBool,
        hang_disconnect: AtomicBool,
        disconnect_started: Notify,
        disconnect_release: Notify,
        /// §11 Stage 2 — how many times `McpTransport::connect` actually
        /// dialed. The whole claim of Stage 2 is that a cache hit dials ZERO
        /// times and a subsequent tool call dials exactly once; an unchanged
        /// `tools`/`resources` field on the served state would pass even if
        /// the mock dialed and the result were silently discarded, so the
        /// call COUNT is the only thing that actually proves it.
        connect_calls: AtomicUsize,
        block_connect: AtomicBool,
        panic_connect: AtomicBool,
        panic_initialize: AtomicBool,
        panic_list_tools: AtomicBool,
        connect_started: Notify,
        connect_release: Notify,
        block_resource_templates: AtomicBool,
        resource_templates_started: Notify,
        resource_templates_release: Notify,
        inbound_txs: TestMutex<HashMap<ConnId, mpsc::Sender<Bytes>>>,
        tool_call_peers: TestMutex<HashMap<ConnId, mpsc::Receiver<Bytes>>>,
        listen_writer_failures_remaining: AtomicUsize,
        connect_failures_remaining: AtomicUsize,
        disconnect_failures_remaining: AtomicUsize,
        /// §11 Stage 2 — how many times `McpTransport::disconnect` actually
        /// ran, so a test can prove a `Cached` server's teardown does NOT
        /// touch the transport (it has no live connection registered).
        disconnect_calls: AtomicUsize,
    }

    impl BridgeMock {
        fn new(tool_names: &[&str]) -> Self {
            let tools = tool_names
                .iter()
                .map(|t| McpToolDto {
                    // Emit the empty `<server>` token, exactly like the posix
                    // transport's `list_tools` (mcp.rs:397) does.
                    full_name: format!("mcp____{t}"),
                    server_name: String::new(),
                    tool_name: (*t).to_string(),
                    description: format!("{t} tool"),
                    input_schema: serde_json::json!({"type": "object"}),
                    output_schema: None,
                    annotations: None,
                    icons: Vec::new(),
                    meta: None,
                    search_hint: None,
                    always_load: None,
                    requires_user_interaction: false,
                })
                .collect();
            Self {
                tools,
                resources: Vec::new(),
                prompts: Vec::new(),
                resource_templates: Vec::new(),
                resources_capability: AtomicBool::new(false),
                prompts_capability: AtomicBool::new(false),
                list_resource_templates_fails: AtomicBool::new(false),
                list_resources_fails: AtomicBool::new(false),
                list_prompts_fails: AtomicBool::new(false),
                drivable_calls: false,
                modern_connect: false,
                templates_calls: AtomicUsize::new(0),
                conns: TestMutex::new(HashMap::new()),
                list_tools_fails: AtomicBool::new(false),
                block_list_tools: AtomicBool::new(false),
                list_tools_started: Notify::new(),
                list_tools_release: Notify::new(),
                disconnect_fails: AtomicBool::new(false),
                block_disconnect: AtomicBool::new(false),
                hang_disconnect: AtomicBool::new(false),
                disconnect_started: Notify::new(),
                disconnect_release: Notify::new(),
                connect_calls: AtomicUsize::new(0),
                block_connect: AtomicBool::new(false),
                panic_connect: AtomicBool::new(false),
                panic_initialize: AtomicBool::new(false),
                panic_list_tools: AtomicBool::new(false),
                connect_started: Notify::new(),
                connect_release: Notify::new(),
                block_resource_templates: AtomicBool::new(false),
                resource_templates_started: Notify::new(),
                resource_templates_release: Notify::new(),
                inbound_txs: TestMutex::new(HashMap::new()),
                tool_call_peers: TestMutex::new(HashMap::new()),
                listen_writer_failures_remaining: AtomicUsize::new(0),
                connect_failures_remaining: AtomicUsize::new(0),
                disconnect_failures_remaining: AtomicUsize::new(0),
                disconnect_calls: AtomicUsize::new(0),
            }
        }

        fn with_drivable_calls(tool_names: &[&str]) -> Self {
            Self {
                drivable_calls: true,
                ..Self::new(tool_names)
            }
        }

        fn with_modern_drivable_calls(tool_names: &[&str]) -> Self {
            Self {
                drivable_calls: true,
                modern_connect: true,
                ..Self::new(tool_names)
            }
        }

        /// A mock whose server advertises the `resources` capability and
        /// answers `resources/templates/list` with `templates` (§26a).
        fn with_resource_templates(templates: Vec<platform_api::McpResourceTemplateDto>) -> Self {
            let mock = Self::new(&[]);
            mock.resources_capability.store(true, Ordering::SeqCst);
            Self {
                resource_templates: templates,
                ..mock
            }
        }

        fn with_catalogs(
            tool_names: &[&str],
            resources: Vec<McpResourceDto>,
            prompts: Vec<McpPromptDto>,
        ) -> Self {
            let mock = Self::new(tool_names);
            mock.resources_capability
                .store(!resources.is_empty(), Ordering::SeqCst);
            mock.prompts_capability
                .store(!prompts.is_empty(), Ordering::SeqCst);
            Self {
                resources,
                prompts,
                ..mock
            }
        }

        /// Same as [`Self::new`], but each tool carries a caller-supplied
        /// `inputSchema` so the §20a connect-path decision can be driven.
        fn with_tool_schemas(tools: &[(&str, serde_json::Value)]) -> Self {
            let mut mock = Self::new(&[]);
            mock.tools = tools
                .iter()
                .map(|(name, schema)| McpToolDto {
                    full_name: format!("mcp____{name}"),
                    server_name: String::new(),
                    tool_name: (*name).to_string(),
                    description: format!("{name} tool"),
                    input_schema: schema.clone(),
                    output_schema: None,
                    annotations: None,
                    icons: Vec::new(),
                    meta: None,
                    search_hint: None,
                    always_load: None,
                    requires_user_interaction: false,
                })
                .collect();
            mock
        }

        fn take_tool_call_peer(
            &self,
            id: ConnId,
        ) -> Option<(mpsc::Sender<Bytes>, mpsc::Receiver<Bytes>)> {
            let inbound = self.inbound_txs.lock().unwrap().remove(&id)?;
            let peer = self.tool_call_peers.lock().unwrap().remove(&id)?;
            Some((inbound, peer))
        }
    }

    #[async_trait]
    impl McpTransport for BridgeMock {
        async fn connect(&self, _s: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
            self.connect_calls.fetch_add(1, Ordering::SeqCst);
            self.connect_started.notify_one();
            if self.block_connect.load(Ordering::SeqCst) {
                self.connect_release.notified().await;
            }
            assert!(
                !self.panic_connect.load(Ordering::SeqCst),
                "bridge mock forced panic in connect"
            );
            if self.connect_failures_remaining.load(Ordering::SeqCst) > 0 {
                let _ = self.connect_failures_remaining.fetch_update(
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                    |remaining| (remaining > 0).then_some(remaining - 1),
                );
                return Err(McpError::Connection(
                    "bridge mock forced connect failure".into(),
                ));
            }
            let id = ConnId::new();
            if self.drivable_calls {
                let (connection, inbound_tx, peer_rx) = drivable_connection();
                self.inbound_txs.lock().unwrap().insert(id, inbound_tx);
                if self.listen_writer_failures_remaining.load(Ordering::SeqCst) > 0 {
                    let _ = self.listen_writer_failures_remaining.fetch_update(
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                        |remaining| (remaining > 0).then_some(remaining - 1),
                    );
                } else {
                    self.tool_call_peers.lock().unwrap().insert(id, peer_rx);
                }
                self.conns.lock().unwrap().insert(id, connection);
            } else {
                self.conns.lock().unwrap().insert(id, paired_connection());
            }
            Ok(McpRawConnection { connection_id: id })
        }
        async fn connect_and_initialize(
            &self,
            spec: &McpTransportSpec,
            _options: platform_api::McpConnectOptions,
        ) -> Result<platform_api::McpConnectResult, McpError> {
            let connection = self.connect(spec).await?;
            let capabilities = match std::panic::AssertUnwindSafe(self.initialize(&connection))
                .catch_unwind()
                .await
            {
                Ok(Ok(capabilities)) => capabilities,
                Ok(Err(error)) => {
                    let _ = self.disconnect(connection.connection_id).await;
                    return Err(error);
                }
                Err(payload) => {
                    let _ = self.disconnect(connection.connection_id).await;
                    std::panic::resume_unwind(payload);
                }
            };
            let negotiated = if self.modern_connect {
                platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Modern,
                    version: "2026-07-28".into(),
                }
            } else {
                platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                }
            };
            Ok(platform_api::McpConnectResult {
                connection,
                capabilities,
                negotiated,
            })
        }
        async fn initialize(
            &self,
            _c: &McpRawConnection,
        ) -> Result<ServerCapabilitiesDto, McpError> {
            assert!(
                !self.panic_initialize.load(Ordering::SeqCst),
                "bridge mock forced panic in initialize"
            );
            Ok(ServerCapabilitiesDto {
                tools: true,
                resources: self.resources_capability.load(Ordering::SeqCst),
                prompts: self.prompts_capability.load(Ordering::SeqCst),
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            })
        }
        async fn list_resource_templates(
            &self,
            _c: &McpRawConnection,
        ) -> Result<Vec<platform_api::McpResourceTemplateDto>, McpError> {
            self.templates_calls.fetch_add(1, Ordering::SeqCst);
            self.resource_templates_started.notify_one();
            if self.block_resource_templates.load(Ordering::SeqCst) {
                self.resource_templates_release.notified().await;
            }
            if self.list_resource_templates_fails.load(Ordering::SeqCst) {
                // What a server with no `resources/templates/list` handler
                // really replies: JSON-RPC -32601, which the posix transport
                // flattens through `map_call_err` into `McpError::Internal`.
                return Err(McpError::Internal(
                    "MCP error -32601: Method not found".into(),
                ));
            }
            Ok(self.resource_templates.clone())
        }
        async fn list_tools(&self, _c: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
            self.list_tools_started.notify_one();
            if self.block_list_tools.load(Ordering::SeqCst) {
                self.list_tools_release.notified().await;
            }
            assert!(
                !self.panic_list_tools.load(Ordering::SeqCst),
                "bridge mock forced panic in list_tools"
            );
            if self.list_tools_fails.load(Ordering::SeqCst) {
                return Err(McpError::Internal("list tools failed".into()));
            }
            Ok(self.tools.clone())
        }
        async fn list_resources(
            &self,
            _c: &McpRawConnection,
        ) -> Result<Vec<McpResourceDto>, McpError> {
            if self.list_resources_fails.load(Ordering::SeqCst) {
                return Err(McpError::Internal("list resources failed".into()));
            }
            Ok(self.resources.clone())
        }
        async fn list_prompts(&self, _c: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
            if self.list_prompts_fails.load(Ordering::SeqCst) {
                return Err(McpError::Internal("list prompts failed".into()));
            }
            Ok(self.prompts.clone())
        }
        async fn call_tool(
            &self,
            _c: &McpRawConnection,
            _t: &str,
            _i: Value,
        ) -> Result<McpToolResultDto, McpError> {
            Ok(McpToolResultDto {
                content: Value::Null,
                is_error: false,
                ..Default::default()
            })
        }
        async fn read_resource(
            &self,
            _c: &McpRawConnection,
            _u: &str,
        ) -> Result<McpResourceContentDto, McpError> {
            Err(McpError::Internal("not implemented".into()))
        }
        async fn ping(&self, _id: ConnId) -> Result<(), McpError> {
            Ok(())
        }
        async fn notifications(
            &self,
            _c: &McpRawConnection,
        ) -> Result<McpNotificationStream, McpError> {
            // Never exercised by these tests (the registry's connect path does
            // not subscribe to notifications).
            unreachable!("notifications not used by bridge/FQN tests")
        }
        async fn handle_elicitation(
            &self,
            _c: &McpRawConnection,
            _r: ElicitRequestDto,
        ) -> Result<ElicitResultDto, McpError> {
            Err(McpError::Internal("not implemented".into()))
        }
        async fn disconnect(&self, id: ConnId) -> Result<(), McpError> {
            self.disconnect_calls.fetch_add(1, Ordering::SeqCst);
            self.disconnect_started.notify_one();
            if self.hang_disconnect.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if self.block_disconnect.load(Ordering::SeqCst) {
                self.disconnect_release.notified().await;
            }
            if self
                .disconnect_failures_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    if remaining > 0 {
                        Some(remaining - 1)
                    } else {
                        None
                    }
                })
                .is_ok()
            {
                return Err(McpError::Internal("disconnect failed".into()));
            }
            if self.disconnect_fails.load(Ordering::SeqCst) {
                return Err(McpError::Internal("disconnect failed".into()));
            }
            self.inbound_txs.lock().unwrap().remove(&id);
            self.tool_call_peers.lock().unwrap().remove(&id);
            self.conns.lock().unwrap().remove(&id);
            Ok(())
        }
        fn supported_transports(&self) -> Vec<McpTransportKind> {
            vec![McpTransportKind::Stdio]
        }
    }

    impl RawConnectionProvider for BridgeMock {
        fn connection_for(&self, id: ConnId) -> Option<Arc<Connection>> {
            self.conns.lock().unwrap().get(&id).cloned()
        }
    }

    /// Same-process transport used to prove that an `InProcess` catalog is
    /// callable without manufacturing a JSON-RPC client solely for dispatch.
    struct DirectInProcessMock {
        connection_id: ConnId,
        calls: TestMutex<Vec<(String, Value)>>,
    }

    impl DirectInProcessMock {
        fn new() -> Self {
            Self {
                connection_id: ConnId::new(),
                calls: TestMutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl McpTransport for DirectInProcessMock {
        async fn connect(&self, spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
            assert!(
                matches!(spec, McpTransportSpec::InProcess { registry_key } if registry_key == "local_apps")
            );
            Ok(McpRawConnection {
                connection_id: self.connection_id,
            })
        }

        async fn initialize(
            &self,
            _conn: &McpRawConnection,
        ) -> Result<ServerCapabilitiesDto, McpError> {
            Ok(ServerCapabilitiesDto {
                tools: true,
                resources: false,
                prompts: false,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            })
        }

        async fn list_tools(&self, _conn: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
            Ok(vec![McpToolDto {
                server_name: String::new(),
                tool_name: "list".into(),
                description: "list local apps".into(),
                input_schema: serde_json::json!({"type":"object"}),
                output_schema: None,
                annotations: None,
                icons: Vec::new(),
                meta: None,
                full_name: String::new(),
                search_hint: None,
                always_load: Some(true),
                requires_user_interaction: false,
            }])
        }

        async fn list_resources(
            &self,
            _conn: &McpRawConnection,
        ) -> Result<Vec<McpResourceDto>, McpError> {
            Ok(Vec::new())
        }

        async fn list_prompts(
            &self,
            _conn: &McpRawConnection,
        ) -> Result<Vec<McpPromptDto>, McpError> {
            Ok(Vec::new())
        }

        async fn call_tool(
            &self,
            _conn: &McpRawConnection,
            tool: &str,
            input: Value,
        ) -> Result<McpToolResultDto, McpError> {
            self.calls
                .lock()
                .unwrap()
                .push((tool.into(), input.clone()));
            Ok(McpToolResultDto {
                content: serde_json::json!([{"type":"text","text":"ok"}]),
                structured_content: Some(serde_json::json!({"tool": tool, "input": input})),
                is_error: false,
                ..Default::default()
            })
        }

        async fn read_resource(
            &self,
            _conn: &McpRawConnection,
            _uri: &str,
        ) -> Result<McpResourceContentDto, McpError> {
            Err(McpError::Internal("resources disabled".into()))
        }

        async fn ping(&self, _connection_id: ConnId) -> Result<(), McpError> {
            Ok(())
        }

        async fn notifications(
            &self,
            _conn: &McpRawConnection,
        ) -> Result<McpNotificationStream, McpError> {
            unreachable!("direct registry connections do not subscribe to JSON-RPC notifications")
        }

        async fn handle_elicitation(
            &self,
            _conn: &McpRawConnection,
            _request: ElicitRequestDto,
        ) -> Result<ElicitResultDto, McpError> {
            Err(McpError::Internal("elicitation disabled".into()))
        }

        async fn disconnect(&self, _connection_id: ConnId) -> Result<(), McpError> {
            Ok(())
        }

        fn supported_transports(&self) -> Vec<McpTransportKind> {
            vec![McpTransportKind::InProcess]
        }
    }

    struct NoopWake;

    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    type DiscoveryCacheEnvGuard = crate::discovery_cache::TestEnvGuard;

    async fn apply_lagged_tool_recovery(
        registry: &McpRegistry,
        active: &mut std::collections::HashSet<McpConnectionId>,
    ) -> Result<(), McpError> {
        let snapshot = registry.catalog_refresh_snapshot().await;
        *active = snapshot
            .iter()
            .filter(|change| change.kind == McpCatalogKind::Tools)
            .map(|change| change.connection_id)
            .collect();
        for change in snapshot {
            if change.kind == McpCatalogKind::Tools {
                if let Some(connection_id) = registry.refresh_catalog(&change).await? {
                    active.insert(connection_id);
                }
            }
        }
        Ok(())
    }

    async fn drive_cleanup_retry_attempts_for_test() {
        for delay_ms in [10, 20, 40, 80, 160] {
            tokio::time::advance(Duration::from_millis(delay_ms)).await;
            tokio::task::yield_now().await;
            tokio::time::advance(cleanup_disconnect_timeout()).await;
            tokio::task::yield_now().await;
        }
    }

    fn spawn_tools_list_response(
        peer_tx: mpsc::Sender<Bytes>,
        mut peer_rx: mpsc::Receiver<Bytes>,
        tool_name: &'static str,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let request_frame = tokio::time::timeout(Duration::from_secs(2), peer_rx.recv())
                .await
                .expect("tools/list request within timeout")
                .expect("tools/list frame");
            let request: Value = serde_json::from_slice(&request_frame).expect("tools/list json");
            assert_eq!(request["method"], "tools/list");
            let mut response = serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"].clone(),
                "result": {
                    "tools": [{
                        "name": tool_name,
                        "description": "fresh",
                        "inputSchema": {"type": "object"}
                    }]
                }
            }))
            .expect("tools/list response");
            response.push(b'\n');
            peer_tx
                .send(Bytes::from(response))
                .await
                .expect("send tools/list response");
        })
    }

    fn spawn_tool_call_response(
        peer_tx: mpsc::Sender<Bytes>,
        mut peer_rx: mpsc::Receiver<Bytes>,
        tool_name: &'static str,
        input: Value,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let request_frame = tokio::time::timeout(Duration::from_secs(2), peer_rx.recv())
                .await
                .expect("tools/call request within timeout")
                .expect("tools/call frame");
            let request: Value = serde_json::from_slice(&request_frame).expect("tools/call json");
            assert_eq!(request["method"], "tools/call");
            assert_eq!(request["params"]["name"], tool_name);
            assert_eq!(request["params"]["arguments"], input);
            let mut response = serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"].clone(),
                "result": {
                    "content": [{"type": "text", "text": "ok"}],
                    "structuredContent": {"tool": tool_name, "input": request["params"]["arguments"].clone()},
                    "isError": false
                }
            }))
            .expect("tools/call response");
            response.push(b'\n');
            peer_tx
                .send(Bytes::from(response))
                .await
                .expect("send tools/call response");
        })
    }

    fn spawn_tool_call_auth_then_success(
        mock: Arc<BridgeMock>,
        first_connection_id: Option<ConnId>,
        tool_name: &'static str,
        input: Value,
        auth_status: u16,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let first_connection_id = if let Some(id) = first_connection_id {
                id
            } else {
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        if let Some(id) = mock.conns.lock().unwrap().keys().next().copied() {
                            break id;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("first live connection appears")
            };
            let (first_tx, mut first_rx) = mock
                .take_tool_call_peer(first_connection_id)
                .expect("first tool-call peer");
            let first_input = input.clone();
            let first_request = tokio::time::timeout(Duration::from_secs(2), async move {
                let request_frame = first_rx.recv().await.expect("first tools/call frame");
                let request: Value =
                    serde_json::from_slice(&request_frame).expect("first tools/call json");
                assert_eq!(request["method"], "tools/call");
                assert_eq!(request["params"]["name"], tool_name);
                assert_eq!(request["params"]["arguments"], first_input);
                request
            })
            .await
            .expect("first tools/call request");
            let mut first_response = serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": first_request["id"].clone(),
                "error": {
                    "code": -32000,
                    "message": format!(
                        "MCP_HTTP_STATUS={auth_status};WWW_AUTHENTICATE=Bearer realm=\"mcp\""
                    )
                }
            }))
            .expect("auth error response");
            first_response.push(b'\n');
            first_tx
                .send(Bytes::from(first_response))
                .await
                .expect("send auth error response");

            let second_connection_id = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(id) = mock
                        .conns
                        .lock()
                        .unwrap()
                        .keys()
                        .copied()
                        .find(|id| *id != first_connection_id)
                    {
                        break id;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("reconnect publishes a second live connection");
            let (second_tx, second_rx) = mock
                .take_tool_call_peer(second_connection_id)
                .expect("second tool-call peer");
            spawn_tool_call_response(second_tx, second_rx, tool_name, input)
                .await
                .expect("second tools/call responder");
        })
    }

    fn spawn_tool_call_auth_then_auth(
        mock: Arc<BridgeMock>,
        first_connection_id: Option<ConnId>,
        tool_name: &'static str,
        input: Value,
        auth_status: u16,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let first_connection_id = if let Some(id) = first_connection_id {
                id
            } else {
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        if let Some(id) = mock.conns.lock().unwrap().keys().next().copied() {
                            break id;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("first live connection appears")
            };
            let (first_tx, mut first_rx) = mock
                .take_tool_call_peer(first_connection_id)
                .expect("first tool-call peer");
            let first_input = input.clone();
            let first_request = tokio::time::timeout(Duration::from_secs(2), async move {
                let request_frame = first_rx.recv().await.expect("first tools/call frame");
                let request: Value =
                    serde_json::from_slice(&request_frame).expect("first tools/call json");
                assert_eq!(request["method"], "tools/call");
                assert_eq!(request["params"]["name"], tool_name);
                assert_eq!(request["params"]["arguments"], first_input);
                request
            })
            .await
            .expect("first tools/call request");
            let mut first_response = serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": first_request["id"].clone(),
                "error": {
                    "code": -32000,
                    "message": format!(
                        "MCP_HTTP_STATUS={auth_status};WWW_AUTHENTICATE=Bearer realm=\"mcp\""
                    )
                }
            }))
            .expect("first auth error response");
            first_response.push(b'\n');
            first_tx
                .send(Bytes::from(first_response))
                .await
                .expect("send first auth error response");

            let second_connection_id = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(id) = mock
                        .conns
                        .lock()
                        .unwrap()
                        .keys()
                        .copied()
                        .find(|id| *id != first_connection_id)
                    {
                        break id;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("reconnect publishes a second live connection");
            let (second_tx, mut second_rx) = mock
                .take_tool_call_peer(second_connection_id)
                .expect("second tool-call peer");
            let second_request = tokio::time::timeout(Duration::from_secs(2), async move {
                let request_frame = second_rx.recv().await.expect("second tools/call frame");
                let request: Value =
                    serde_json::from_slice(&request_frame).expect("second tools/call json");
                assert_eq!(request["method"], "tools/call");
                assert_eq!(request["params"]["name"], tool_name);
                assert_eq!(request["params"]["arguments"], input);
                request
            })
            .await
            .expect("second tools/call request");
            let mut second_response = serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": second_request["id"].clone(),
                "error": {
                    "code": -32000,
                    "message": format!(
                        "MCP_HTTP_STATUS={auth_status};WWW_AUTHENTICATE=Bearer realm=\"mcp\""
                    )
                }
            }))
            .expect("second auth error response");
            second_response.push(b'\n');
            second_tx
                .send(Bytes::from(second_response))
                .await
                .expect("send second auth error response");
        })
    }

    fn spawn_tool_call_session_expired_then_success(
        mock: Arc<BridgeMock>,
        first_connection_id: Option<ConnId>,
        tool_name: &'static str,
        input: Value,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let first_connection_id = if let Some(id) = first_connection_id {
                id
            } else {
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        if let Some(id) = mock.conns.lock().unwrap().keys().next().copied() {
                            break id;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("first live connection appears")
            };
            let (first_tx, mut first_rx) = mock
                .take_tool_call_peer(first_connection_id)
                .expect("first tool-call peer");
            let first_input = input.clone();
            let first_request = tokio::time::timeout(Duration::from_secs(2), async move {
                let request_frame = first_rx.recv().await.expect("first tools/call frame");
                let request: Value =
                    serde_json::from_slice(&request_frame).expect("first tools/call json");
                assert_eq!(request["method"], "tools/call");
                assert_eq!(request["params"]["name"], tool_name);
                assert_eq!(request["params"]["arguments"], first_input);
                request
            })
            .await
            .expect("first tools/call request");
            let mut first_response = serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": first_request["id"].clone(),
                "error": {
                    "code": -32001,
                    "message": "MCP_HTTP_STATUS=404;WWW_AUTHENTICATE="
                }
            }))
            .expect("session expired response");
            first_response.push(b'\n');
            first_tx
                .send(Bytes::from(first_response))
                .await
                .expect("send session expired response");

            let second_connection_id = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if let Some(id) = mock
                        .conns
                        .lock()
                        .unwrap()
                        .keys()
                        .copied()
                        .find(|id| *id != first_connection_id)
                    {
                        break id;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("reconnect publishes a second live connection");
            let (second_tx, second_rx) = mock
                .take_tool_call_peer(second_connection_id)
                .expect("second tool-call peer");
            spawn_tool_call_response(second_tx, second_rx, tool_name, input)
                .await
                .expect("second tools/call responder");
        })
    }

    fn cfg(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            spec: McpTransportSpec::Stdio {
                command: "echo".into(),
                args: vec![],
                env: HashMap::new(),
            },
            scope: ConfigScope::Settings(protocol::SettingsScope::Project),
            disabled: false,
            timeout_ms: None,
            always_load: false,
            discovery_cache: None,
            tools: Vec::new(),
            tool_permissions: std::collections::BTreeMap::new(),
            config_error: None,
            metadata: Default::default(),
        }
    }

    /// [`cfg`] with a remote `http` spec, so the §20a per-server gate has a
    /// hostname to resolve.
    fn http_cfg(name: &str, url: &str) -> McpServerConfig {
        McpServerConfig {
            spec: McpTransportSpec::Http {
                url: url.into(),
                headers: Default::default(),
                headers_helper: None,
                oauth: None,
            },
            ..cfg(name)
        }
    }

    fn resource(name: &str, uri: &str) -> McpResourceDto {
        McpResourceDto {
            uri: uri.into(),
            name: name.into(),
            description: None,
            mime_type: None,
            meta: None,
        }
    }

    fn prompt(name: &str) -> McpPromptDto {
        McpPromptDto {
            name: name.into(),
            description: None,
            arguments: Vec::new(),
        }
    }

    fn legacy_negotiated() -> platform_api::McpNegotiatedProtocol {
        platform_api::McpNegotiatedProtocol {
            era: platform_api::McpProtocolEra::Legacy,
            version: "2025-11-25".into(),
        }
    }

    fn modern_negotiated() -> platform_api::McpNegotiatedProtocol {
        platform_api::McpNegotiatedProtocol {
            era: platform_api::McpProtocolEra::Modern,
            version: "2026-07-28".into(),
        }
    }

    fn caps(tools: bool, resources: bool, prompts: bool) -> ServerCapabilitiesDto {
        ServerCapabilitiesDto {
            tools,
            resources,
            prompts,
            directory_read: false,
            logging: false,
            experimental: HashMap::new(),
            extensions: HashMap::new(),
        }
    }

    // ---- Batch 1: client bridge --------------------------------------------

    #[tokio::test]
    async fn connect_registers_client_when_raw_conn_present() {
        let tool_names: Vec<String> = (0..17).map(|index| format!("blocked-{index}")).collect();
        let tool_name_refs: Vec<&str> = tool_names.iter().map(String::as_str).collect();
        let mock = Arc::new(BridgeMock::new(&tool_name_refs));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        );
        registry.connect(cfg("mock")).await.unwrap();
        assert!(
            registry.get_client("mock").await.is_some(),
            "a live McpClient must be registered when raw_conn is present"
        );
    }

    #[tokio::test]
    async fn conditional_remove_requires_the_expected_config_snapshot() {
        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
        let expected = http_cfg("srv", "https://mcp.example.com/old");
        let newer = http_cfg("srv", "https://mcp.example.com/new");
        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Disconnected {
                config: newer.clone(),
                last_error: None,
            },
        );

        assert!(!registry
            .remove_without_revoking_auth_if_config("srv", &expected)
            .await
            .expect("conditional removal mismatch must be observable"));
        assert!(registry.connections.read().await.contains_key("srv"));

        assert!(registry
            .remove_without_revoking_auth_if_config("srv", &newer)
            .await
            .expect("matching conditional removal must succeed"));
        assert!(!registry.connections.read().await.contains_key("srv"));
    }

    #[tokio::test]
    async fn guarded_conditional_remove_rechecks_generation_after_lifecycle_wait() {
        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = Arc::new(McpRegistry::new(mock as Arc<dyn McpTransport>));
        let expected = http_cfg("srv", "https://mcp.example.com/v1");
        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Disconnected {
                config: expected.clone(),
                last_error: None,
            },
        );

        let lifecycle = registry.lifecycle_lock("srv");
        let held = lifecycle.lock().await;
        let current = Arc::new(AtomicBool::new(true));
        let guard_calls = Arc::new(AtomicUsize::new(0));
        let guard = {
            let current = current.clone();
            let guard_calls = guard_calls.clone();
            Arc::new(move || {
                guard_calls.fetch_add(1, Ordering::SeqCst);
                current.load(Ordering::SeqCst)
            }) as Arc<McpOperationGuard>
        };
        let removal = {
            let registry = registry.clone();
            let guard = guard.clone();
            tokio::spawn(async move {
                registry
                    .remove_without_revoking_auth_if_config_guarded("srv", &expected, guard)
                    .await
            })
        };

        // The job is queued behind the lifecycle lock. Invalidate its
        // generation before releasing that lock; the registry callback must
        // run after lock acquisition and prevent the stale removal.
        tokio::task::yield_now().await;
        current.store(false, Ordering::SeqCst);
        drop(held);
        assert!(!removal
            .await
            .expect("guarded removal task")
            .expect("guarded removal result"));
        assert_eq!(guard_calls.load(Ordering::SeqCst), 1);
        assert!(registry.connections.read().await.contains_key("srv"));
    }

    #[tokio::test]
    async fn guarded_conditional_remove_rechecks_generation_after_snapshot_wait() {
        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = Arc::new(McpRegistry::new(mock as Arc<dyn McpTransport>));
        let expected = http_cfg("srv", "https://mcp.example.com/v1");
        let mut held = registry.connections.write().await;
        held.insert(
            "srv".into(),
            McpConnectionState::Disconnected {
                config: expected.clone(),
                last_error: None,
            },
        );
        let current = Arc::new(AtomicBool::new(true));
        let guard_calls = Arc::new(AtomicUsize::new(0));
        let guard = {
            let current = current.clone();
            let guard_calls = guard_calls.clone();
            Arc::new(move || {
                guard_calls.fetch_add(1, Ordering::SeqCst);
                current.load(Ordering::SeqCst)
            }) as Arc<McpOperationGuard>
        };
        let removal = {
            let registry = registry.clone();
            tokio::spawn(async move {
                registry
                    .remove_without_revoking_auth_if_config_guarded("srv", &expected, guard)
                    .await
            })
        };
        // Wait for the guard to pass under the lifecycle lock, then revoke
        // intent while its config snapshot is blocked by our state writer.
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while guard_calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("removal reached the snapshot wait");
        current.store(false, Ordering::SeqCst);
        drop(held);
        assert!(!removal
            .await
            .expect("removal task")
            .expect("removal result"));
        assert!(registry.connections.read().await.contains_key("srv"));
    }

    #[tokio::test]
    async fn guarded_connect_rejects_before_live_publish_and_disconnects_discovery() {
        let publish_hook = Arc::new(TestPauseHook::default());
        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = Arc::new(
            McpRegistry::new(mock.clone() as Arc<dyn McpTransport>)
                .with_pause_before_client_publish(publish_hook.clone()),
        );
        let current = Arc::new(AtomicBool::new(true));
        let guard = {
            let current = current.clone();
            Arc::new(move || current.load(Ordering::SeqCst)) as Arc<McpOperationGuard>
        };

        let connect = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.connect_if_current(cfg("srv"), guard).await })
        };
        tokio::time::timeout(Duration::from_secs(2), mock.connect_started.notified())
            .await
            .expect("guarded connect reaches transport");
        tokio::time::timeout(Duration::from_secs(2), publish_hook.entered.notified())
            .await
            .expect("guarded connect reaches the publish gate");
        current.store(false, Ordering::SeqCst);
        publish_hook.release.notify_one();

        assert!(connect
            .await
            .expect("guarded connect task")
            .expect("guarded connect result")
            .is_none());
        assert!(registry.connections.read().await.is_empty());
        assert!(mock.conns.lock().unwrap().is_empty());
        assert_eq!(mock.disconnect_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rejected_guarded_connect_does_not_remove_a_newer_disconnected_config() {
        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
        let desired = cfg("srv");
        let newer = http_cfg("srv", "https://mcp.example.com/newer");
        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Disconnected {
                config: newer.clone(),
                last_error: None,
            },
        );
        let guard = Arc::new(|| false) as Arc<McpOperationGuard>;

        assert!(registry
            .connect_if_current(desired, guard)
            .await
            .expect("rejected connect result")
            .is_none());
        assert!(matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Disconnected { config, .. })
                if McpRegistry::same_config_snapshot(config, &newer)
        ));
    }

    #[tokio::test]
    async fn stale_guarded_connect_error_does_not_publish_disconnected_state() {
        let mock = Arc::new(BridgeMock::new(&[]));
        mock.block_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(McpRegistry::new(mock.clone() as Arc<dyn McpTransport>));
        let current = Arc::new(AtomicBool::new(true));
        let guard = {
            let current = current.clone();
            Arc::new(move || current.load(Ordering::SeqCst)) as Arc<McpOperationGuard>
        };
        let connect = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.connect_if_current(cfg("srv"), guard).await })
        };
        tokio::time::timeout(Duration::from_secs(2), mock.connect_started.notified())
            .await
            .expect("blocked connect reaches transport");

        // The transport fails only after this generation is superseded. The
        // stale error path must clean its own Connecting marker, not publish
        // a Disconnected state carrying the old config.
        current.store(false, Ordering::SeqCst);
        mock.list_tools_fails.store(true, Ordering::SeqCst);
        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();

        assert!(connect
            .await
            .expect("stale connect task")
            .expect("stale connect result")
            .is_none());
        assert!(registry.connections.read().await.is_empty());
        assert!(mock.conns.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn guarded_disabled_connect_seeds_disconnected_without_dialing() {
        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = McpRegistry::new(mock.clone() as Arc<dyn McpTransport>);
        let mut disabled = cfg("srv");
        disabled.disabled = true;
        let guard = Arc::new(|| true) as Arc<McpOperationGuard>;

        assert!(registry
            .connect_if_current(disabled.clone(), guard)
            .await
            .expect("disabled guarded connect")
            .is_none());
        assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 0);
        assert!(matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Disconnected { config, last_error: None })
                if config.disabled && McpRegistry::same_config_snapshot(config, &disabled)
        ));
    }

    #[tokio::test]
    async fn inprocess_server_dispatches_directly_without_jsonrpc_client() {
        let transport = Arc::new(DirectInProcessMock::new());
        let registry = McpRegistry::new(transport.clone());
        registry
            .connect(McpServerConfig {
                name: "local_apps".into(),
                spec: McpTransportSpec::InProcess {
                    registry_key: "local_apps".into(),
                },
                scope: ConfigScope::Settings(protocol::SettingsScope::Managed),
                disabled: false,
                timeout_ms: None,
                always_load: true,
                discovery_cache: None,
                tools: Vec::new(),
                tool_permissions: std::collections::BTreeMap::new(),
                config_error: None,
                metadata: Default::default(),
            })
            .await
            .unwrap();

        assert!(registry.get_client("local_apps").await.is_none());
        assert!(registry.has_callable_server("local_apps").await);
        let result = registry
            .call_tool_with_auth_retry(
                "local_apps",
                "mcp__local_apps__list",
                serde_json::json!({"limit": 5}),
                None,
                None,
            )
            .await
            .unwrap();

        assert_eq!(
            result.structured_content,
            Some(serde_json::json!({"tool":"list","input":{"limit":5}}))
        );
        assert_eq!(
            *transport.calls.lock().unwrap(),
            vec![("list".into(), serde_json::json!({"limit": 5}))]
        );
    }

    #[tokio::test]
    async fn tools_list_failure_keeps_transport_connected_and_emits_success_before_catalogs_finish()
    {
        let _capture = test_telemetry_capture_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_test_telemetry_events();

        let mock = Arc::new(BridgeMock::new(&["read"]));
        mock.list_tools_fails.store(true, Ordering::SeqCst);
        mock.block_list_tools.store(true, Ordering::SeqCst);
        let registry = Arc::new(McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        ));

        let connect = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.connect(cfg("mock")).await })
        };
        tokio::time::timeout(Duration::from_secs(2), mock.list_tools_started.notified())
            .await
            .expect("connect must reach list_tools");

        let midflight = take_test_telemetry_events();
        assert!(
            midflight
                .iter()
                .any(|event| event.name == telemetry::tengu::mcp::SERVER_CONNECTION_SUCCEEDED),
            "initialize success must emit before catalog completion"
        );
        assert!(
            !midflight
                .iter()
                .any(|event| event.name == telemetry::tengu::mcp::TOOLS_LISTED
                    && event.payload.get("tool_count") == Some(&serde_json::json!(17))),
            "tools/list event must wait for the catalog result"
        );

        mock.list_tools_release.notify_one();
        let connection_id = connect.await.expect("join").expect("connect succeeds");
        let tail_events = take_test_telemetry_events();
        let mut events = midflight.clone();
        events.extend(tail_events);
        assert!(
            events
                .iter()
                .any(|event| event.name == telemetry::tengu::mcp::SERVER_CONNECTION_SUCCEEDED),
            "successful initialize must be recorded"
        );
        assert!(
            !events
                .iter()
                .any(|event| event.name == telemetry::tengu::mcp::SERVER_CONNECTION_FAILED),
            "catalog failure must not be reported as a connection failure"
        );
        assert!(
            events.iter().any(|event| {
                event.name == telemetry::tengu::mcp::DEGRADED
                    && event.payload.get("reason") == Some(&serde_json::json!("tools_list_failed"))
            }),
            "tools/list failure must emit the exact degraded reason"
        );
        assert!(
            !events
                .iter()
                .any(|event| event.name == telemetry::tengu::mcp::TOOLS_LISTED
                    && event.payload.get("tool_count") == Some(&serde_json::json!(17))),
            "failed tools/list must not emit tools_listed"
        );
        let states = registry.connections.read().await;
        let Some(McpConnectionState::Connected {
            connection_id: current,
            tools,
            ..
        }) = states.get("mock")
        else {
            panic!("expected Connected state after partial catalog failure");
        };
        assert_eq!(*current, connection_id);
        assert!(tools.is_empty(), "failed tools catalog falls back to empty");
        assert!(
            mock.conns.lock().unwrap().contains_key(&connection_id),
            "the initialized transport must remain live"
        );
    }

    #[tokio::test]
    async fn resources_list_failure_keeps_tools_and_marks_only_resources_failed() {
        let _capture = test_telemetry_capture_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_test_telemetry_events();

        let mock = Arc::new(BridgeMock::with_catalogs(
            &["read"],
            vec![resource("guide", "file:///guide.md")],
            Vec::new(),
        ));
        mock.resources_capability.store(true, Ordering::SeqCst);
        mock.list_resources_fails.store(true, Ordering::SeqCst);
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        );

        registry
            .connect(cfg("mock"))
            .await
            .expect("connect succeeds");

        let states = registry.connections.read().await;
        let Some(McpConnectionState::Connected {
            tools, resources, ..
        }) = states.get("mock")
        else {
            panic!("expected Connected state");
        };
        assert_eq!(tools.len(), 1, "successful tools catalog is retained");
        assert!(
            resources.is_empty(),
            "failed resources catalog falls back to empty on first connect"
        );
        drop(states);

        let events = take_test_telemetry_events();
        assert!(
            events.iter().any(|event| {
                event.name == telemetry::tengu::mcp::DEGRADED
                    && event.payload.get("reason")
                        == Some(&serde_json::json!("resources_list_failed"))
            }),
            "resources/list failure must emit the exact degraded reason"
        );
        assert!(
            events
                .iter()
                .any(|event| event.name == telemetry::tengu::mcp::TOOLS_LISTED),
            "successful tools/list must still emit tools_listed"
        );
    }

    #[tokio::test]
    async fn prompts_list_failure_keeps_other_catalogs_live() {
        let _capture = test_telemetry_capture_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_test_telemetry_events();

        let mock = Arc::new(BridgeMock::with_catalogs(
            &["read"],
            vec![resource("guide", "file:///guide.md")],
            vec![prompt("draft")],
        ));
        mock.list_prompts_fails.store(true, Ordering::SeqCst);
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        );

        registry
            .connect(cfg("mock"))
            .await
            .expect("connect succeeds");

        let states = registry.connections.read().await;
        let Some(McpConnectionState::Connected {
            tools,
            resources,
            prompts,
            ..
        }) = states.get("mock")
        else {
            panic!("expected Connected state");
        };
        assert_eq!(tools.len(), 1);
        assert_eq!(resources.len(), 1);
        assert!(
            prompts.is_empty(),
            "failed prompts catalog falls back to empty"
        );
        drop(states);

        let events = take_test_telemetry_events();
        assert!(
            events.iter().any(|event| {
                event.name == telemetry::tengu::mcp::DEGRADED
                    && event.payload.get("reason")
                        == Some(&serde_json::json!("prompts_list_failed"))
            }),
            "prompts/list failure must emit the exact degraded reason"
        );
    }

    #[tokio::test]
    async fn mixed_catalog_failures_keep_successful_catalogs_and_preserve_cached_failed_slices() {
        let _capture = test_telemetry_capture_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_test_telemetry_events();

        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry_with_catalog(
            &store,
            &cache_key,
            1_000_000,
            ServerCapabilitiesDto {
                tools: true,
                resources: true,
                prompts: true,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            vec![McpToolDto {
                server_name: "srv".into(),
                tool_name: "cached_tool".into(),
                description: "cached tool".into(),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                annotations: None,
                icons: Vec::new(),
                meta: None,
                full_name: "mcp__srv__cached_tool".into(),
                search_hint: None,
                always_load: None,
                requires_user_interaction: false,
            }],
            vec![resource("cached_guide", "file:///cached.md")],
            vec![prompt("cached_prompt")],
        );

        let mock = Arc::new(BridgeMock::with_catalogs(
            &["live_tool"],
            vec![resource("live_guide", "file:///live.md")],
            vec![prompt("live_prompt")],
        ));
        mock.list_tools_fails.store(true, Ordering::SeqCst);
        mock.list_prompts_fails.store(true, Ordering::SeqCst);
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        let cached_id = registry
            .connect(cfg.clone())
            .await
            .expect("cache hit connect");
        let live_id = registry
            .ensure_dialed_from_cache("srv")
            .await
            .expect("partial live discovery still succeeds");
        assert_ne!(live_id, cached_id);

        let states = registry.connections.read().await;
        let Some(McpConnectionState::Connected {
            connection_id,
            tools,
            resources,
            prompts,
            ..
        }) = states.get("srv")
        else {
            panic!("expected Connected state after lazy dial");
        };
        assert_eq!(*connection_id, live_id);
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.tool_name.as_str())
                .collect::<Vec<_>>(),
            vec!["cached_tool"],
            "failed tools/list must preserve the cached safe catalog"
        );
        assert_eq!(
            resources
                .iter()
                .map(|resource| resource.name.as_str())
                .collect::<Vec<_>>(),
            vec!["live_guide"],
            "successful resources/list must replace the cached catalog"
        );
        assert_eq!(
            prompts
                .iter()
                .map(|prompt| prompt.name.as_str())
                .collect::<Vec<_>>(),
            vec!["cached_prompt"],
            "failed prompts/list must preserve the cached safe catalog"
        );
        drop(states);
        drop(env);

        let events = take_test_telemetry_events();
        let degraded_reasons: Vec<&str> = events
            .iter()
            .filter(|event| event.name == telemetry::tengu::mcp::DEGRADED)
            .filter_map(|event| {
                event
                    .payload
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
            })
            .collect();
        assert!(degraded_reasons.contains(&"tools_list_failed"));
        assert!(degraded_reasons.contains(&"prompts_list_failed"));
        assert!(
            !events
                .iter()
                .any(|event| event.name == telemetry::tengu::mcp::SERVER_CONNECTION_FAILED),
            "partial live discovery must not demote the initialized connection to a failure"
        );
    }

    #[tokio::test]
    async fn connect_and_disconnect_publish_tool_partition_lifecycle() {
        let mock = Arc::new(BridgeMock::new(&["read"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        );
        let mut changes = registry.subscribe_catalog_changes();

        let connection_id = registry.connect(cfg("mock")).await.unwrap();
        let connected = tokio::time::timeout(Duration::from_secs(2), changes.recv())
            .await
            .expect("connect catalog event within timeout")
            .expect("catalog sender remains live");
        assert_eq!(
            connected,
            McpCatalogChanged {
                server_name: "mock".into(),
                connection_id,
                retired_connection_id: None,
                kind: McpCatalogKind::Tools,
                telemetry_cause: None,
            }
        );

        registry.disconnect("mock").await.unwrap();
        let disconnected = tokio::time::timeout(Duration::from_secs(2), changes.recv())
            .await
            .expect("disconnect catalog event within timeout")
            .expect("catalog sender remains live");
        assert_eq!(
            disconnected,
            McpCatalogChanged {
                server_name: "mock".into(),
                connection_id,
                retired_connection_id: Some(connection_id),
                kind: McpCatalogKind::Tools,
                telemetry_cause: None,
            }
        );
    }

    #[tokio::test]
    async fn lazy_dial_success_retires_cached_partition_then_disconnect_retires_live_partition() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));
        let mut changes = registry.subscribe_catalog_changes();

        let cached_id = registry.connect(cfg).await.expect("cache hit connect");
        let cached_connected = changes.recv().await.expect("cached connect event");
        assert_eq!(
            cached_connected,
            McpCatalogChanged {
                server_name: "srv".into(),
                connection_id: cached_id,
                retired_connection_id: None,
                kind: McpCatalogKind::Tools,
                telemetry_cause: None,
            }
        );

        let live_id = registry
            .ensure_dialed_from_cache("srv")
            .await
            .expect("foreground lazy dial");
        let live_connected = changes.recv().await.expect("live replacement event");
        assert_eq!(
            live_connected,
            McpCatalogChanged {
                server_name: "srv".into(),
                connection_id: live_id,
                retired_connection_id: Some(cached_id),
                kind: McpCatalogKind::Tools,
                telemetry_cause: None,
            }
        );

        registry.disconnect("srv").await.expect("disconnect");
        let disconnected = changes.recv().await.expect("disconnect event");
        assert_eq!(
            disconnected,
            McpCatalogChanged {
                server_name: "srv".into(),
                connection_id: live_id,
                retired_connection_id: Some(live_id),
                kind: McpCatalogKind::Tools,
                telemetry_cause: None,
            }
        );
        drop(env);
    }

    #[tokio::test]
    async fn lazy_dial_partial_catalog_failure_replaces_cached_partition_without_zombies() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        mock.list_tools_fails.store(true, Ordering::SeqCst);
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));
        let mut changes = registry.subscribe_catalog_changes();

        let cached_id = registry.connect(cfg).await.expect("cache hit connect");
        let _ = changes.recv().await.expect("cached connect event");
        let live_id = registry
            .ensure_dialed_from_cache("srv")
            .await
            .expect("partial catalog failure must still promote the live transport");
        let replaced = changes.recv().await.expect("live replacement event");
        assert_eq!(
            replaced,
            McpCatalogChanged {
                server_name: "srv".into(),
                connection_id: live_id,
                retired_connection_id: Some(cached_id),
                kind: McpCatalogKind::Tools,
                telemetry_cause: None,
            }
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), changes.recv())
                .await
                .is_err(),
            "partial lazy-dial replacement must publish exactly one follow-up event"
        );
        drop(env);
    }

    #[tokio::test]
    async fn late_waiters_join_one_detached_cached_lazy_upgrade_owner() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        mock.block_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        registry.connect(cfg).await.expect("cache hit connect");
        let first = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.ensure_connected_client("srv").await })
        };
        mock.connect_started.notified().await;

        let second = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.ensure_connected_client("srv").await })
        };
        assert!(
            matches!(
                registry.connections.read().await.get("srv"),
                Some(McpConnectionState::Connecting { .. })
            ),
            "late waiter must join the detached lazy-upgrade owner while state is Connecting"
        );

        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        let first_client = tokio::time::timeout(Duration::from_secs(2), first)
            .await
            .expect("first waiter finishes")
            .expect("first join succeeds")
            .expect("first waiter receives client");
        let second_client = tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .expect("second waiter finishes")
            .expect("second join succeeds")
            .expect("second waiter receives client");
        drop(env);

        assert!(
            Arc::ptr_eq(&first_client, &second_client),
            "both waiters must receive the same published live client"
        );
        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            1,
            "late waiters must share one lazy-upgrade dial"
        );
        assert!(
            matches!(
                registry.connections.read().await.get("srv"),
                Some(McpConnectionState::Connected { .. })
            ),
            "the detached owner must publish a final Connected state"
        );
    }

    #[tokio::test]
    async fn cancelling_the_initiating_waiter_does_not_cancel_the_detached_lazy_upgrade_owner() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        mock.block_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        registry.connect(cfg).await.expect("cache hit connect");
        let waiter = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.ensure_connected_client("srv").await })
        };
        mock.connect_started.notified().await;
        waiter.abort();
        let join = waiter.await;
        assert!(join.is_err_and(|error| error.is_cancelled()));

        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        let later_client = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match registry.ensure_connected_client("srv").await {
                    Ok(client) => break client,
                    Err(_) => tokio::task::yield_now().await,
                }
            }
        })
        .await
        .expect("later waiter succeeds after detached owner publishes");
        drop(env);

        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            1,
            "cancelling the initiating waiter must not trigger a second lazy-upgrade dial"
        );
        assert!(
            registry.lazy_upgrade_slots.read().await.is_empty(),
            "completed detached owner must retire its coordination slot"
        );
        assert!(
            matches!(
                registry.connections.read().await.get("srv"),
                Some(McpConnectionState::Connected { .. })
            ),
            "owner completion must not strand the server in Connecting after waiter cancellation"
        );
        assert!(
            registry.get_client("srv").await.is_some() && Arc::strong_count(&later_client) >= 1,
            "the published live client remains available after the initiating waiter is dropped"
        );
    }

    #[tokio::test]
    async fn disconnecting_a_foreground_lazy_upgrade_clears_connecting_and_unblocks_waiters() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        mock.block_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        registry.connect(cfg).await.expect("cache hit connect");
        let waiter = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.ensure_connected_client("srv").await })
        };
        mock.connect_started.notified().await;

        registry.disconnect("srv").await.expect("disconnect wins");
        let waiter_result = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("waiter finishes")
            .expect("join succeeds");
        let waiter_result = match waiter_result {
            Ok(_) => panic!("waiter should fail after disconnect"),
            Err(error) => error,
        };
        assert!(
            matches!(
                registry.connections.read().await.get("srv"),
                Some(McpConnectionState::Stopped { .. })
            ),
            "disconnect must not leave a slot-owned Connecting zombie behind"
        );
        assert!(
            registry.lazy_upgrade_slots.read().await.is_empty(),
            "disconnect must remove the lazy-upgrade slot"
        );
        assert!(
            waiter_result
                .to_string()
                .contains("MCP server \"srv\" is no longer cached"),
            "disconnected waiters must resolve with a terminal error"
        );

        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            matches!(
                registry.connections.read().await.get("srv"),
                Some(McpConnectionState::Stopped { .. })
            ),
            "late owner completion must not revive the disconnected server"
        );
        drop(env);
    }

    #[tokio::test]
    async fn removing_a_foreground_lazy_upgrade_drops_state_and_unblocks_waiters() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        mock.block_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        registry.connect(cfg).await.expect("cache hit connect");
        let waiter = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.ensure_connected_client("srv").await })
        };
        mock.connect_started.notified().await;

        registry
            .remove_without_revoking_auth("srv")
            .await
            .expect("remove wins");
        let waiter_result = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("waiter finishes")
            .expect("join succeeds");
        let waiter_result = match waiter_result {
            Ok(_) => panic!("waiter should fail after remove"),
            Err(error) => error,
        };
        assert!(
            registry.connections.read().await.get("srv").is_none(),
            "remove must drop the slot-owned Connecting state entirely"
        );
        assert!(
            waiter_result
                .to_string()
                .contains("MCP server \"srv\" is no longer cached"),
            "removed waiters must resolve with a terminal error"
        );

        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            registry.connections.read().await.get("srv").is_none(),
            "late owner completion must not recreate a removed server"
        );
        drop(env);
    }

    #[tokio::test]
    async fn failed_disconnect_preserves_live_state_client_and_catalog() {
        let mock = Arc::new(BridgeMock::new(&["read"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        );
        let mut changes = registry.subscribe_catalog_changes();

        let connection_id = registry.connect(cfg("mock")).await.unwrap();
        changes.recv().await.unwrap();
        mock.disconnect_fails.store(true, Ordering::SeqCst);

        let error = registry.disconnect("mock").await.unwrap_err();
        assert!(error.to_string().contains("disconnect failed"));
        assert!(registry.get_client("mock").await.is_some());
        let conns = registry.connections.read().await;
        assert!(matches!(
            conns.get("mock"),
            Some(McpConnectionState::Connected {
                connection_id: current,
                ..
            }) if *current == connection_id
        ));
        drop(conns);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), changes.recv())
                .await
                .is_err(),
            "a failed teardown must not retire the live tool partition"
        );
    }

    #[tokio::test]
    async fn live_disable_retires_connection_without_revoking_reconnect_config() {
        let mock = Arc::new(BridgeMock::new(&["read"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        );
        let mut changes = registry.subscribe_catalog_changes();
        let connection_id = registry.connect(cfg("mock")).await.unwrap();
        changes.recv().await.unwrap();

        assert_eq!(
            registry.set_disabled("mock", true).await.unwrap(),
            Some(platform_api::McpActionState::Disabled)
        );
        assert!(registry.get_client("mock").await.is_none());
        assert_eq!(
            registry.action_states().await,
            vec![("mock".to_string(), platform_api::McpActionState::Disabled)]
        );
        let retired = changes.recv().await.unwrap();
        assert_eq!(retired.retired_connection_id, Some(connection_id));

        assert_eq!(
            registry.set_disabled("mock", false).await.unwrap(),
            Some(platform_api::McpActionState::Connected)
        );
        assert!(registry.get_client("mock").await.is_some());
        assert_eq!(
            registry.action_states().await,
            vec![("mock".to_string(), platform_api::McpActionState::Connected)]
        );
    }

    #[tokio::test]
    async fn cached_disable_retires_connection_without_touching_transport() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));
        let mut changes = registry.subscribe_catalog_changes();

        let cached_id = registry.connect(cfg).await.expect("fresh cache hit");
        let initial = changes.recv().await.expect("cached registration");
        assert_eq!(initial.connection_id, cached_id);
        assert_eq!(initial.retired_connection_id, None);

        assert_eq!(
            registry.set_disabled("srv", true).await.unwrap(),
            Some(platform_api::McpActionState::Disabled)
        );
        assert_eq!(
            mock.disconnect_calls.load(Ordering::SeqCst),
            0,
            "cached disable must not call transport disconnect"
        );
        let retired = changes.recv().await.expect("cached retirement");
        assert_eq!(retired.retired_connection_id, Some(cached_id));
        drop(env);
    }

    #[tokio::test]
    async fn failed_live_disable_preserves_connected_generation() {
        let mock = Arc::new(BridgeMock::new(&["read"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        );
        let connection_id = registry.connect(cfg("mock")).await.unwrap();
        mock.disconnect_fails.store(true, Ordering::SeqCst);

        assert!(registry.set_disabled("mock", true).await.is_err());
        assert!(registry.get_client("mock").await.is_some());
        let conns = registry.connections.read().await;
        assert!(matches!(
            conns.get("mock"),
            Some(McpConnectionState::Connected {
                connection_id: current,
                config,
                ..
            }) if *current == connection_id && !config.disabled
        ));
    }

    #[tokio::test]
    async fn failed_atomic_config_replacement_preserves_connected_generation() {
        let mock = Arc::new(BridgeMock::new(&["read"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        );
        let original = cfg("mock");
        let connection_id = registry.connect(original.clone()).await.unwrap();
        mock.connect_failures_remaining.store(1, Ordering::SeqCst);
        let mut replacement = original.clone();
        replacement.timeout_ms = Some(42_000);

        assert!(registry
            .replace_config_atomically(replacement)
            .await
            .is_err());
        assert!(registry.get_client("mock").await.is_some());
        let connections = registry.connections.read().await;
        assert!(matches!(
            connections.get("mock"),
            Some(McpConnectionState::Connected {
                connection_id: current,
                config,
                ..
            }) if *current == connection_id && McpRegistry::same_config_snapshot(config, &original)
        ));
    }

    #[tokio::test]
    async fn set_disabled_noop_false_preserves_a_foreground_lazy_upgrade_slot() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        mock.block_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        registry.connect(cfg).await.expect("fresh cache hit");
        let owner = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
        };
        mock.connect_started.notified().await;

        let slot = registry
            .lazy_upgrade_slot("srv")
            .await
            .expect("foreground slot registered");
        assert_eq!(registry.set_disabled("srv", false).await.unwrap(), None);
        let same_slot = registry
            .lazy_upgrade_slot("srv")
            .await
            .expect("no-op disable keeps foreground slot");
        assert!(Arc::ptr_eq(&slot, &same_slot));

        let joiner = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if mock.connect_calls.load(Ordering::SeqCst) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("no-op disable must not redial");

        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        let owner_id = tokio::time::timeout(Duration::from_secs(2), owner)
            .await
            .expect("owner waiter completes")
            .expect("owner join succeeds")
            .expect("owner receives live id");
        let joiner_id = tokio::time::timeout(Duration::from_secs(2), joiner)
            .await
            .expect("joiner completes")
            .expect("joiner task succeeds")
            .expect("joiner receives same live id");
        drop(env);

        assert_eq!(owner_id, joiner_id);
        assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn set_disabled_noop_false_preserves_a_background_lazy_upgrade_slot() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 1_000_000);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        mock.block_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        let cached_id = registry.connect(cfg).await.expect("stale cache hit");
        mock.connect_started.notified().await;

        let slot = registry
            .lazy_upgrade_slot("srv")
            .await
            .expect("background slot registered");
        assert_eq!(registry.set_disabled("srv", false).await.unwrap(), None);
        let same_slot = registry
            .lazy_upgrade_slot("srv")
            .await
            .expect("no-op disable keeps background slot");
        assert!(Arc::ptr_eq(&slot, &same_slot));

        let waiter = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if mock.connect_calls.load(Ordering::SeqCst) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("foreground join must share the existing background owner");

        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        let live_id = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("foreground waiter completes")
            .expect("waiter join succeeds")
            .expect("foreground waiter receives live id");
        drop(env);

        assert_ne!(live_id, cached_id);
        assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn disabling_a_foreground_lazy_upgrade_invalidates_the_slot_and_prevents_revival() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        mock.block_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        registry.connect(cfg).await.expect("fresh cache hit");
        let waiter = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
        };
        mock.connect_started.notified().await;

        assert_eq!(
            registry.set_disabled("srv", true).await.unwrap(),
            Some(platform_api::McpActionState::Disabled)
        );
        assert!(
            registry.lazy_upgrade_slots.read().await.is_empty(),
            "disable must invalidate the foreground slot"
        );
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("waiter completes after disable")
            .expect("waiter join succeeds")
            .unwrap_err();

        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if mock.disconnect_calls.load(Ordering::SeqCst) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("rejected live transport cleaned up after disable");
        drop(env);

        assert!(matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Disconnected { config, .. }) if config.disabled
        ));
    }

    #[tokio::test]
    async fn enabling_a_server_that_with_a_catalog_failure_still_settles_connected() {
        let mock = Arc::new(BridgeMock::new(&["read"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        );
        registry.connect(cfg("mock")).await.unwrap();
        // Disabling settles as `Disabled`.
        assert_eq!(
            registry.set_disabled("mock", true).await.unwrap(),
            Some(platform_api::McpActionState::Disabled)
        );
        // The next connect still succeeds: the transport/initialize path is
        // authoritative, while tools/list now degrades in place.
        mock.list_tools_fails.store(true, Ordering::SeqCst);
        // Re-enabling must still NOT return `Err`, and with independent
        // catalog fetches it now settles `Connected` with an empty tools slice.
        assert_eq!(
            registry.set_disabled("mock", false).await.unwrap(),
            Some(platform_api::McpActionState::Connected)
        );
        assert_eq!(
            registry.action_states().await,
            vec![("mock".to_string(), platform_api::McpActionState::Connected)]
        );
        assert!(matches!(
            registry.connections.read().await.get("mock"),
            Some(McpConnectionState::Connected { tools, .. }) if tools.is_empty()
        ));
    }

    #[tokio::test]
    async fn failed_action_servers_reports_sanitized_errors_sorted() {
        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        );
        {
            let mut conns = registry.connections.write().await;
            conns.insert(
                "zeta".into(),
                McpConnectionState::Failed {
                    config: cfg("zeta"),
                    // The quotes must be stripped by the `xLt` sanitizer.
                    error: "he said \"boom\"".into(),
                    attempts: 3,
                },
            );
            conns.insert(
                "alpha".into(),
                McpConnectionState::Failed {
                    config: cfg("alpha"),
                    error: "Blocked by enterprise managed policy".into(),
                    attempts: 1,
                },
            );
        }
        // Sorted by name; each error run through `sanitize_diagnostic`.
        assert_eq!(
            registry.failed_action_servers().await,
            vec![
                (
                    "alpha".to_string(),
                    Some("Blocked by enterprise managed policy".to_string())
                ),
                ("zeta".to_string(), Some("he said boom".to_string())),
            ]
        );
    }

    #[test]
    fn sanitize_diagnostic_strips_quotes_controls_and_caps_at_200() {
        // Angle brackets, `"`, `;` and the fancy-quote set collapse to spaces.
        assert_eq!(sanitize_diagnostic("hello \"world\""), "hello world");
        assert_eq!(sanitize_diagnostic("angle <b> ; semi"), "angle b semi");
        // Control chars (`\p{Cc}`) collapse to a single space.
        assert_eq!(sanitize_diagnostic("a\u{0000}\u{0007}b"), "a b");
        // Runs of whitespace collapse and the result is trimmed.
        assert_eq!(sanitize_diagnostic("  trim   me  "), "trim me");
        // Capped at 200 chars + `…` (U+2026).
        let capped = sanitize_diagnostic(&"x".repeat(250));
        assert_eq!(capped.chars().count(), 201);
        assert!(capped.ends_with('\u{2026}'));
    }

    #[tokio::test]
    async fn slow_disconnect_does_not_block_registry_snapshots() {
        let mock = Arc::new(BridgeMock::new(&["read"]));
        let registry = Arc::new(McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        ));
        registry.connect(cfg("mock")).await.unwrap();
        mock.block_disconnect.store(true, Ordering::SeqCst);

        let disconnect = {
            let registry = Arc::clone(&registry);
            tokio::spawn(async move { registry.disconnect("mock").await })
        };
        mock.disconnect_started.notified().await;

        let snapshot = tokio::time::timeout(Duration::from_millis(50), registry.snapshot())
            .await
            .expect("transport teardown must not hold the connection-state lock");
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].status, platform_api::McpStatus::Connected);

        mock.disconnect_release.notify_one();
        disconnect.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn reconnect_one_does_not_clobber_an_already_connected_server() {
        let mock = Arc::new(BridgeMock::new(&["read"]));
        let registry = Arc::new(McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        ));
        let id1 = registry.connect(cfg("mock")).await.unwrap();

        // A stale reconnect candidate fires for a server that is now Connected
        // (a concurrent connect won the race). Under the fix it aborts on the
        // Connected guard instead of overwriting the live connection with
        // `Reconnecting` — which would strand `id1`, unreachable for teardown.
        Arc::clone(&registry).reconnect_one(cfg("mock")).await;

        let conns = registry.connections.read().await;
        match conns.get("mock") {
            Some(McpConnectionState::Connected { connection_id, .. }) => {
                assert_eq!(
                    *connection_id, id1,
                    "the live connection id must be preserved"
                );
            }
            other => panic!("expected the connection to stay Connected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reconnect_one_reconnects_a_disconnected_server() {
        // Regression: the reconnect path still works for a genuinely
        // reconnect-worthy (non-Connected) state.
        let mock = Arc::new(BridgeMock::new(&["read"]));
        let registry = Arc::new(McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        ));
        registry.connections.write().await.insert(
            "mock".into(),
            McpConnectionState::Disconnected {
                config: cfg("mock"),
                last_error: Some("boom".into()),
            },
        );

        Arc::clone(&registry).reconnect_one(cfg("mock")).await;

        let conns = registry.connections.read().await;
        match conns.get("mock") {
            Some(McpConnectionState::Connected { .. }) => {}
            other => panic!("expected reconnect to reach Connected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn inbound_tools_list_changed_is_forwarded_with_connection_generation() {
        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
        let (connection, peer_tx, _peer_rx) = drivable_connection();
        let connection_id = ConnId::new();
        let mut changes = registry.subscribe_catalog_changes();
        registry.spawn_catalog_change_listener(
            "srv".into(),
            connection_id,
            connection,
            platform_api::McpNegotiatedProtocol {
                era: platform_api::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            ServerCapabilitiesDto {
                tools: true,
                resources: false,
                prompts: false,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            None,
        );

        let mut frame = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/tools/list_changed",
            "params": {}
        }))
        .unwrap();
        frame.push(b'\n');
        peer_tx.send(Bytes::from(frame)).await.unwrap();

        let change = tokio::time::timeout(Duration::from_secs(2), changes.recv())
            .await
            .expect("catalog notification within timeout")
            .expect("catalog sender remains live");
        assert_eq!(
            change,
            McpCatalogChanged {
                server_name: "srv".into(),
                connection_id,
                retired_connection_id: None,
                kind: McpCatalogKind::Tools,
                telemetry_cause: Some("notification"),
            }
        );
    }

    #[tokio::test]
    async fn inbound_resources_list_changed_is_forwarded() {
        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
        let (connection, peer_tx, _peer_rx) = drivable_connection();
        let connection_id = ConnId::new();
        let mut changes = registry.subscribe_catalog_changes();
        registry.spawn_catalog_change_listener(
            "srv".into(),
            connection_id,
            connection,
            platform_api::McpNegotiatedProtocol {
                era: platform_api::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            ServerCapabilitiesDto {
                tools: false,
                resources: true,
                prompts: false,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            None,
        );

        let mut frame = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/resources/list_changed",
            "params": {}
        }))
        .unwrap();
        frame.push(b'\n');
        peer_tx.send(Bytes::from(frame)).await.unwrap();

        let change = tokio::time::timeout(Duration::from_secs(2), changes.recv())
            .await
            .expect("catalog notification within timeout")
            .expect("catalog sender remains live");
        assert_eq!(
            change,
            McpCatalogChanged {
                server_name: "srv".into(),
                connection_id,
                retired_connection_id: None,
                kind: McpCatalogKind::Resources,
                telemetry_cause: Some("notification"),
            }
        );
    }

    #[tokio::test]
    async fn lagged_catalog_listener_recovers_all_supported_catalogs_for_current_generation() {
        let _capture = test_telemetry_capture_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_test_telemetry_events();
        let pause = Arc::new(Notify::new());
        set_catalog_change_listener_pause_for_test(Some(pause.clone()));

        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = Arc::new(McpRegistry::new(mock as Arc<dyn McpTransport>));
        let (connection, peer_tx, mut peer_rx) = drivable_connection();
        let connection_id = ConnId::new();
        let client = Arc::new(
            McpClient::new(
                "srv",
                std::path::PathBuf::from("/tmp/work"),
                connection.clone(),
            )
            .await,
        );
        registry.clients.write().await.insert(
            "srv".into(),
            RegisteredClient {
                connection_id: Some(connection_id),
                client,
            },
        );
        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Connected {
                config: cfg("srv"),
                connection_id,
                capabilities: ServerCapabilitiesDto {
                    tools: true,
                    resources: true,
                    prompts: true,
                    directory_read: false,
                    logging: false,
                    experimental: HashMap::new(),
                    extensions: HashMap::new(),
                },
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![McpToolDto {
                    server_name: "srv".into(),
                    tool_name: "old".into(),
                    description: "old".into(),
                    input_schema: serde_json::json!({"type":"object"}),
                    output_schema: None,
                    annotations: None,
                    icons: Vec::new(),
                    meta: None,
                    full_name: "mcp__srv__old".into(),
                    search_hint: None,
                    always_load: None,
                    requires_user_interaction: false,
                }],
                resources: vec![resource("old-resource", "file:///old.md")],
                resource_templates: Vec::new(),
                prompts: vec![prompt("old-prompt")],
                connected_at: SystemTime::now(),
            },
        );
        let mut changes = registry.subscribe_catalog_changes();
        registry.spawn_catalog_change_listener(
            "srv".into(),
            connection_id,
            connection,
            platform_api::McpNegotiatedProtocol {
                era: platform_api::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            ServerCapabilitiesDto {
                tools: true,
                resources: true,
                prompts: true,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            None,
        );

        for _ in 0..300 {
            let mut frame = serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/tools/list_changed",
                "params": {}
            }))
            .unwrap();
            frame.push(b'\n');
            peer_tx.send(Bytes::from(frame)).await.unwrap();
        }
        set_catalog_change_listener_pause_for_test(None);
        pause.notify_waiters();

        let refresh_registry = registry.clone();
        let refresh = tokio::spawn(async move {
            let mut seen = Vec::new();
            for _ in 0..3 {
                let change = tokio::time::timeout(Duration::from_secs(2), changes.recv())
                    .await
                    .expect("lag recovery change within timeout")
                    .expect("catalog change sender remains live");
                assert_eq!(change.connection_id, connection_id);
                assert_eq!(change.telemetry_cause, Some(LISTEN_REOPEN_CAUSE));
                refresh_registry
                    .refresh_catalog(&change)
                    .await
                    .expect("refresh succeeds");
                seen.push(change.kind);
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(100), changes.recv())
                    .await
                    .is_err(),
                "lag recovery must coalesce buffered notifications into one authoritative refresh set"
            );
            seen
        });

        for _ in 0..3 {
            let request_frame = tokio::time::timeout(Duration::from_secs(2), peer_rx.recv())
                .await
                .expect("catalog refresh request within timeout")
                .expect("catalog refresh frame");
            let request: Value = serde_json::from_slice(&request_frame).unwrap();
            let method = request["method"].as_str().expect("request method");
            let result = match method {
                "tools/list" => serde_json::json!({
                    "tools": [{
                        "name": "fresh-tool",
                        "description": "fresh",
                        "inputSchema": {"type": "object"}
                    }]
                }),
                "prompts/list" => serde_json::json!({
                    "prompts": [{
                        "name": "fresh-prompt",
                        "description": "fresh",
                        "arguments": []
                    }]
                }),
                "resources/list" => serde_json::json!({
                    "resources": [{
                        "uri": "file:///fresh.md",
                        "name": "fresh-resource"
                    }]
                }),
                other => panic!("unexpected lag recovery request {other}"),
            };
            let mut response = serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"].clone(),
                "result": result
            }))
            .unwrap();
            response.push(b'\n');
            peer_tx.send(Bytes::from(response)).await.unwrap();
        }

        let mut seen: Vec<&'static str> = refresh
            .await
            .expect("lag refresh join")
            .into_iter()
            .map(|kind| match kind {
                McpCatalogKind::Tools => "tools",
                McpCatalogKind::Prompts => "prompts",
                McpCatalogKind::Resources => "resources",
            })
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, vec!["prompts", "resources", "tools"]);
        let conns = registry.connections.read().await;
        let Some(McpConnectionState::Connected {
            tools,
            prompts,
            resources,
            ..
        }) = conns.get("srv")
        else {
            panic!("server must remain connected");
        };
        assert_eq!(tools[0].tool_name, "fresh-tool");
        assert_eq!(prompts[0].name, "fresh-prompt");
        assert_eq!(resources[0].name, "fresh-resource");
    }

    #[tokio::test]
    async fn lagged_catalog_listener_skips_recovery_for_a_replaced_generation() {
        let pause = Arc::new(Notify::new());
        set_catalog_change_listener_pause_for_test(Some(pause.clone()));

        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
        let (connection, peer_tx, _peer_rx) = drivable_connection();
        let old_connection_id = ConnId::new();
        let new_connection_id = ConnId::new();
        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Connected {
                config: cfg("srv"),
                connection_id: old_connection_id,
                capabilities: ServerCapabilitiesDto {
                    tools: true,
                    resources: true,
                    prompts: true,
                    directory_read: false,
                    logging: false,
                    experimental: HashMap::new(),
                    extensions: HashMap::new(),
                },
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: Vec::new(),
                resources: Vec::new(),
                resource_templates: Vec::new(),
                prompts: Vec::new(),
                connected_at: SystemTime::now(),
            },
        );
        let mut changes = registry.subscribe_catalog_changes();
        registry.spawn_catalog_change_listener(
            "srv".into(),
            old_connection_id,
            connection,
            platform_api::McpNegotiatedProtocol {
                era: platform_api::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            ServerCapabilitiesDto {
                tools: true,
                resources: true,
                prompts: true,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            None,
        );

        for _ in 0..300 {
            let mut frame = serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/tools/list_changed",
                "params": {}
            }))
            .unwrap();
            frame.push(b'\n');
            peer_tx.send(Bytes::from(frame)).await.unwrap();
        }
        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Connected {
                config: cfg("srv"),
                connection_id: new_connection_id,
                capabilities: ServerCapabilitiesDto {
                    tools: true,
                    resources: true,
                    prompts: true,
                    directory_read: false,
                    logging: false,
                    experimental: HashMap::new(),
                    extensions: HashMap::new(),
                },
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: Vec::new(),
                resources: Vec::new(),
                resource_templates: Vec::new(),
                prompts: Vec::new(),
                connected_at: SystemTime::now(),
            },
        );
        set_catalog_change_listener_pause_for_test(None);
        pause.notify_waiters();

        assert!(
            tokio::time::timeout(Duration::from_millis(200), changes.recv())
                .await
                .is_err(),
            "lag recovery must not publish authoritative refreshes for a replaced generation"
        );
    }

    #[tokio::test]
    async fn modern_listen_request_is_filtered_and_ack_opens_from_zero() {
        let _capture = test_telemetry_capture_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_test_telemetry_events();

        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
        let (connection, peer_tx, mut peer_rx) = drivable_connection();
        let connection_id = ConnId::new();
        let mut changes = registry.subscribe_catalog_changes();
        registry.spawn_catalog_change_listener(
            "srv".into(),
            connection_id,
            connection,
            modern_negotiated(),
            caps(true, true, false),
            Some(ModernListenOpenTelemetry {
                outcome: telemetry::tengu::mcp::ListenReopenOutcome::OpenedFromZero,
                attempts: 0,
                trigger: telemetry::tengu::mcp::ListenReopenTrigger::Connect,
            }),
        );

        let request_frame = tokio::time::timeout(Duration::from_secs(2), peer_rx.recv())
            .await
            .expect("listen request within timeout")
            .expect("listen request frame");
        let request: Value = serde_json::from_slice(&request_frame).unwrap();
        assert_eq!(request["method"], serde_json::json!("subscriptions/listen"));
        assert_eq!(
            request["params"]["notifications"],
            serde_json::json!({
                "toolsListChanged": true,
                "resourcesListChanged": true,
            })
        );

        let mut ack = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/subscriptions/acknowledged",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/subscriptionId": request["id"].clone(),
                }
            }
        }))
        .unwrap();
        ack.push(b'\n');
        peer_tx.send(Bytes::from(ack)).await.unwrap();

        let mut tool_change = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/tools/list_changed",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/subscriptionId": request["id"].clone(),
                }
            }
        }))
        .unwrap();
        tool_change.push(b'\n');
        peer_tx.send(Bytes::from(tool_change)).await.unwrap();

        let change = tokio::time::timeout(Duration::from_secs(2), changes.recv())
            .await
            .expect("matching modern list_changed")
            .expect("catalog sender remains live");
        assert_eq!(change.connection_id, connection_id);
        assert_eq!(change.kind, McpCatalogKind::Tools);
        assert_eq!(change.telemetry_cause, Some("notification"));

        let events = take_test_telemetry_events();
        assert!(events.iter().any(|event| {
            event.name == telemetry::tengu::mcp::LISTEN_REOPEN
                && event.payload.get("outcome") == Some(&serde_json::json!("opened_from_zero"))
                && event.payload.get("attempts") == Some(&serde_json::json!(0))
                && event.payload.get("trigger") == Some(&serde_json::json!("connect"))
        }));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn modern_graceful_close_reopens_after_extra_delay_and_publishes_refresh_snapshot() {
        let _capture = test_telemetry_capture_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_test_telemetry_events();

        let mut mock = BridgeMock::with_modern_drivable_calls(&["fresh-tool"]);
        mock.resources_capability.store(true, Ordering::SeqCst);
        mock.prompts_capability.store(true, Ordering::SeqCst);
        mock.resources = vec![resource("fresh-resource", "file:///fresh.md")];
        mock.prompts = vec![prompt("fresh-prompt")];
        let mock = Arc::new(mock);
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        );
        let first_connection_id = registry.connect(cfg("srv")).await.unwrap();
        let (first_tx, mut first_rx) = mock
            .take_tool_call_peer(first_connection_id)
            .expect("first listen peer");

        let first_listen = first_rx.recv().await.expect("first listen request");
        let first_request: Value = serde_json::from_slice(&first_listen).unwrap();
        assert_eq!(
            first_request["method"],
            serde_json::json!("subscriptions/listen")
        );
        let mut first_ack = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/subscriptions/acknowledged",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/subscriptionId": first_request["id"].clone(),
                }
            }
        }))
        .unwrap();
        first_ack.push(b'\n');
        first_tx.send(Bytes::from(first_ack)).await.unwrap();
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        let mut changes = registry.subscribe_catalog_changes();
        let mut graceful = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": first_request["id"].clone(),
            "result": { "resultType": "complete" }
        }))
        .unwrap();
        graceful.push(b'\n');
        first_tx.send(Bytes::from(graceful)).await.unwrap();
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        let epsilon = Duration::from_millis(1);
        tokio::time::advance(Duration::from_secs(6) - epsilon).await;
        tokio::task::yield_now().await;
        assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 1);

        tokio::time::advance(epsilon).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 2);

        let second_connection_id = {
            let conns = registry.connections.read().await;
            match conns.get("srv") {
                Some(McpConnectionState::Connected { connection_id, .. }) => *connection_id,
                other => panic!("expected reopened connected state, got {other:?}"),
            }
        };
        assert_ne!(second_connection_id, first_connection_id);

        let (second_tx, mut second_rx) = mock
            .take_tool_call_peer(second_connection_id)
            .expect("second listen peer");
        let second_listen = second_rx.recv().await.expect("second listen request");
        let second_request: Value = serde_json::from_slice(&second_listen).unwrap();
        assert_eq!(
            second_request["method"],
            serde_json::json!("subscriptions/listen")
        );
        let mut second_ack = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/subscriptions/acknowledged",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/subscriptionId": second_request["id"].clone(),
                }
            }
        }))
        .unwrap();
        second_ack.push(b'\n');
        second_tx.send(Bytes::from(second_ack)).await.unwrap();
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        let mut seen = Vec::new();
        for _ in 0..3 {
            let change = tokio::time::timeout(Duration::from_secs(1), changes.recv())
                .await
                .expect("reopen snapshot change")
                .expect("catalog changes remain live");
            seen.push(change);
        }
        assert!(seen.iter().any(|change| {
            change.kind == McpCatalogKind::Tools
                && change.connection_id == second_connection_id
                && change.retired_connection_id == Some(first_connection_id)
                && change.telemetry_cause == Some(LISTEN_REOPEN_CAUSE)
        }));
        assert!(seen.iter().any(|change| {
            change.kind == McpCatalogKind::Prompts
                && change.connection_id == second_connection_id
                && change.telemetry_cause == Some(LISTEN_REOPEN_CAUSE)
        }));
        assert!(seen.iter().any(|change| {
            change.kind == McpCatalogKind::Resources
                && change.connection_id == second_connection_id
                && change.telemetry_cause == Some(LISTEN_REOPEN_CAUSE)
        }));

        let events = take_test_telemetry_events();
        assert!(events.iter().any(|event| {
            event.name == telemetry::tengu::mcp::LISTEN_REOPEN
                && event.payload.get("outcome") == Some(&serde_json::json!("opened_from_zero"))
                && event.payload.get("trigger") == Some(&serde_json::json!("connect"))
        }));
        assert!(events.iter().any(|event| {
            event.name == telemetry::tengu::mcp::LISTEN_REOPEN
                && event.payload.get("outcome") == Some(&serde_json::json!("reopened"))
                && event.payload.get("attempts") == Some(&serde_json::json!(1))
                && event.payload.get("trigger") == Some(&serde_json::json!("graceful"))
        }));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn modern_listen_start_send_failure_reopens_on_remote_path() {
        let _capture = test_telemetry_capture_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_test_telemetry_events();

        let mock = BridgeMock::with_modern_drivable_calls(&["fresh-tool"]);
        mock.listen_writer_failures_remaining
            .store(1, Ordering::SeqCst);
        let mock = Arc::new(mock);
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        );

        let first_connection_id = registry.connect(cfg("srv")).await.unwrap();
        assert!(
            mock.take_tool_call_peer(first_connection_id).is_none(),
            "the first generation's listen writer should be closed before any peer can observe it"
        );
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }

        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 2);

        let second_connection_id = {
            let conns = registry.connections.read().await;
            match conns.get("srv") {
                Some(McpConnectionState::Connected { connection_id, .. }) => *connection_id,
                other => panic!("expected reopened connected state, got {other:?}"),
            }
        };
        assert_ne!(second_connection_id, first_connection_id);

        let (second_tx, mut second_rx) = mock
            .take_tool_call_peer(second_connection_id)
            .expect("second listen peer");
        let second_listen = second_rx.recv().await.expect("second listen request");
        let second_request: Value = serde_json::from_slice(&second_listen).unwrap();
        assert_eq!(
            second_request["method"],
            serde_json::json!("subscriptions/listen")
        );
        let mut second_ack = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/subscriptions/acknowledged",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/subscriptionId": second_request["id"].clone(),
                }
            }
        }))
        .unwrap();
        second_ack.push(b'\n');
        second_tx.send(Bytes::from(second_ack)).await.unwrap();
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        let events = take_test_telemetry_events();
        assert!(events.iter().any(|event| {
            event.name == telemetry::tengu::mcp::LISTEN_REOPEN
                && event.payload.get("outcome") == Some(&serde_json::json!("reopened"))
                && event.payload.get("attempts") == Some(&serde_json::json!(1))
                && event.payload.get("trigger") == Some(&serde_json::json!("remote"))
        }));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn modern_listen_start_send_failure_marks_generation_disconnected_after_retry_budget() {
        let _capture = test_telemetry_capture_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_test_telemetry_events();

        let mock = BridgeMock::with_modern_drivable_calls(&["fresh-tool"]);
        mock.listen_writer_failures_remaining
            .store(1, Ordering::SeqCst);
        let mock = Arc::new(mock);
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        );

        let first_connection_id = registry.connect(cfg("srv")).await.unwrap();
        assert!(
            mock.take_tool_call_peer(first_connection_id).is_none(),
            "the first generation's listen writer should be closed before any peer can observe it"
        );
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        mock.connect_failures_remaining.store(3, Ordering::SeqCst);

        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(2)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(4)).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 4);

        let conns = registry.connections.read().await;
        match conns.get("srv") {
            Some(McpConnectionState::Disconnected {
                last_error: Some(error),
                ..
            }) => {
                assert!(
                    error.contains("bridge mock forced connect failure"),
                    "unexpected terminal error: {error}"
                );
            }
            other => panic!("expected disconnected terminal state, got {other:?}"),
        }
        drop(conns);

        let events = take_test_telemetry_events();
        assert!(events.iter().any(|event| {
            event.name == telemetry::tengu::mcp::LISTEN_REOPEN
                && event.payload.get("outcome") == Some(&serde_json::json!("gave_up"))
                && event.payload.get("attempts") == Some(&serde_json::json!(3))
                && event.payload.get("trigger") == Some(&serde_json::json!("remote"))
        }));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn modern_listener_budget_exhaustion_parks_and_cancellation_gives_up() {
        let _capture = test_telemetry_capture_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_test_telemetry_events();
        set_listener_reopen_park_jitter_for_test(Some(1.0));

        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
        let connection_id = ConnId::new();
        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Connected {
                config: cfg("srv"),
                connection_id,
                capabilities: caps(true, false, false),
                negotiated: modern_negotiated(),
                tools: Vec::new(),
                resources: Vec::new(),
                resource_templates: Vec::new(),
                prompts: Vec::new(),
                connected_at: SystemTime::now(),
            },
        );
        registry.listener_reopen_state.write().await.insert(
            "srv".into(),
            ListenerReopenState {
                delay_index: 0,
                opened_at: Some(tokio::time::Instant::now()),
                reopened_at: vec![
                    tokio::time::Instant::now();
                    LISTENER_REOPEN_MAX_ATTEMPTS_PER_WINDOW
                ],
            },
        );

        let task = tokio::spawn({
            let registry = registry.clone_for_background();
            async move {
                registry
                    .handle_modern_catalog_listener_end(
                        "srv",
                        connection_id,
                        telemetry::tengu::mcp::ListenReopenTrigger::Remote,
                    )
                    .await;
            }
        });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        registry.connections.write().await.remove("srv");
        tokio::time::advance(LISTENER_REOPEN_PARK_POLL).await;
        task.await.expect("budget exhaustion task joins");
        set_listener_reopen_park_jitter_for_test(None);

        let events: Vec<_> = take_test_telemetry_events()
            .into_iter()
            .filter(|event| event.name == telemetry::tengu::mcp::LISTEN_REOPEN)
            .collect();
        assert_eq!(
            events
                .iter()
                .map(|event| event.payload.get("outcome").cloned())
                .collect::<Vec<_>>(),
            vec![
                Some(serde_json::json!("budget_exhausted")),
                Some(serde_json::json!("parked")),
                Some(serde_json::json!("gave_up")),
            ]
        );
        assert!(events
            .iter()
            .all(|event| { event.payload.get("trigger") == Some(&serde_json::json!("remote")) }));
    }

    #[tokio::test]
    async fn registry_notification_subscription_uses_live_client_connection() {
        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
        let (connection, peer_tx, _peer_rx) = drivable_connection();
        let client = Arc::new(
            McpClient::new("srv", std::path::PathBuf::from("/tmp/work"), connection).await,
        );
        registry.register_client("srv", client).await;

        let mut notifications = registry
            .subscribe_notifications("srv")
            .await
            .expect("a live client must expose its notification stream");
        let mut frame = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/resources/list_changed",
            "params": {"cursor": "next"}
        }))
        .unwrap();
        frame.push(b'\n');
        peer_tx.send(Bytes::from(frame)).await.unwrap();

        let notification = tokio::time::timeout(Duration::from_secs(2), notifications.next())
            .await
            .expect("registry subscription must receive a server push")
            .expect("the live connection must remain open");
        assert_eq!(notification.method, "notifications/resources/list_changed");
        assert_eq!(notification.params, serde_json::json!({"cursor": "next"}));
    }

    #[tokio::test]
    async fn refresh_tools_catalog_replaces_connected_snapshot_after_success() {
        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = Arc::new(McpRegistry::new(mock as Arc<dyn McpTransport>));
        let (connection, peer_tx, mut peer_rx) = drivable_connection();
        let client = Arc::new(
            McpClient::new("srv", std::path::PathBuf::from("/tmp/work"), connection).await,
        );
        let connection_id = ConnId::new();
        registry.clients.write().await.insert(
            "srv".into(),
            RegisteredClient {
                connection_id: Some(connection_id),
                client,
            },
        );
        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Connected {
                config: cfg("srv"),
                connection_id,
                capabilities: ServerCapabilitiesDto {
                    tools: true,
                    resources: false,
                    prompts: false,
                    directory_read: false,
                    logging: false,
                    experimental: HashMap::new(),
                    extensions: HashMap::new(),
                },
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: BridgeMock::new(&["old"]).tools,
                resources: Vec::new(),
                resource_templates: Vec::new(),
                prompts: Vec::new(),
                connected_at: SystemTime::now(),
            },
        );

        let change = McpCatalogChanged {
            server_name: "srv".into(),
            connection_id,
            retired_connection_id: None,
            kind: McpCatalogKind::Tools,
            telemetry_cause: None,
        };
        let refresh_registry = registry.clone();
        let refresh = tokio::spawn(async move { refresh_registry.refresh_catalog(&change).await });
        let request_frame = tokio::time::timeout(Duration::from_secs(2), peer_rx.recv())
            .await
            .expect("tools/list request within timeout")
            .expect("tools/list frame");
        let request: Value = serde_json::from_slice(&request_frame).unwrap();
        assert_eq!(request["method"], "tools/list");
        let mut response = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"].clone(),
            "result": {
                "tools": [{
                    "name": "new tool",
                    "description": "fresh",
                    "inputSchema": {"type": "object"}
                }]
            }
        }))
        .unwrap();
        response.push(b'\n');
        peer_tx.send(Bytes::from(response)).await.unwrap();

        assert_eq!(
            refresh.await.unwrap().unwrap(),
            Some(connection_id),
            "current connection generation must be refreshed"
        );
        let conns = registry.connections.read().await;
        let McpConnectionState::Connected { tools, .. } = conns.get("srv").unwrap() else {
            panic!("server must remain connected")
        };
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool_name, "new tool");
        assert_eq!(tools[0].full_name, "mcp__srv__new_tool");
    }

    #[tokio::test]
    async fn get_prompt_rejects_generation_swapped_client_after_validation() {
        let registry = Arc::new(McpRegistry::new(Arc::new(BridgeMock::new(&[]))));
        let (conn_a, mut peer_a) = observable_connection();
        let (conn_b, mut peer_b) = observable_connection();
        let client_a =
            Arc::new(McpClient::new("srv", std::path::PathBuf::from("/tmp/work"), conn_a).await);
        let client_b =
            Arc::new(McpClient::new("srv", std::path::PathBuf::from("/tmp/work"), conn_b).await);
        let old_id = ConnId::new();
        let new_id = ConnId::new();

        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Connected {
                config: cfg("srv"),
                connection_id: old_id,
                capabilities: ServerCapabilitiesDto {
                    tools: false,
                    resources: false,
                    prompts: true,
                    directory_read: false,
                    logging: false,
                    experimental: HashMap::new(),
                    extensions: HashMap::new(),
                },
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: Vec::new(),
                resources: Vec::new(),
                resource_templates: Vec::new(),
                prompts: vec![McpPromptDto {
                    name: "draft".into(),
                    description: None,
                    arguments: Vec::new(),
                }],
                connected_at: SystemTime::now(),
            },
        );
        let mut clients = registry.clients.write().await;
        clients.insert(
            "srv".into(),
            RegisteredClient {
                connection_id: Some(old_id),
                client: client_a,
            },
        );

        let mut get_prompt = std::pin::pin!(registry.get_prompt(
            old_id,
            "draft",
            serde_json::json!({ "topic": "release" })
        ));
        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(get_prompt.as_mut().poll(&mut cx), Poll::Pending));

        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Connected {
                config: cfg("srv"),
                connection_id: new_id,
                capabilities: ServerCapabilitiesDto {
                    tools: false,
                    resources: false,
                    prompts: true,
                    directory_read: false,
                    logging: false,
                    experimental: HashMap::new(),
                    extensions: HashMap::new(),
                },
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: Vec::new(),
                resources: Vec::new(),
                resource_templates: Vec::new(),
                prompts: vec![McpPromptDto {
                    name: "draft".into(),
                    description: None,
                    arguments: Vec::new(),
                }],
                connected_at: SystemTime::now(),
            },
        );
        clients.insert(
            "srv".into(),
            RegisteredClient {
                connection_id: Some(new_id),
                client: client_b,
            },
        );
        drop(clients);

        let error = get_prompt.await.unwrap_err();
        assert!(error.to_string().contains(&format!(
            "MCP prompt connection {old_id} is no longer active"
        )));
        assert!(
            peer_a.try_recv().is_err(),
            "the original generation must not receive prompts/get after replacement",
        );
        assert!(
            peer_b.try_recv().is_err(),
            "the new generation must not receive a stale prompts/get request",
        );
    }

    #[tokio::test]
    async fn cached_prompt_rejects_generation_swapped_after_lazy_dial_upgrade() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry_with_catalog(
            &store,
            &cache_key,
            0,
            ServerCapabilitiesDto {
                tools: false,
                resources: false,
                prompts: true,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            vec![],
            vec![],
            vec![McpPromptDto {
                name: "draft".into(),
                description: Some("draft prompt".into()),
                arguments: Vec::new(),
            }],
        );

        let prompt_mock = Arc::new(PromptBridgeMock::new());
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                prompt_mock.clone() as Arc<dyn McpTransport>,
                prompt_mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );
        let cached_id = registry
            .connect(cfg)
            .await
            .expect("fresh cache hit connect");
        let live_id = registry
            .ensure_dialed_from_cache("srv")
            .await
            .expect("prompts path lazy dial");
        assert_ne!(
            live_id, cached_id,
            "live generation replaces the cached one"
        );

        let (conn_b, mut peer_b) = observable_connection();
        let client_b =
            Arc::new(McpClient::new("srv", std::path::PathBuf::from("/tmp/work"), conn_b).await);
        let new_id = ConnId::new();
        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Connected {
                config: http_cfg("srv", "https://mcp.example.com/v1"),
                connection_id: new_id,
                capabilities: ServerCapabilitiesDto {
                    tools: false,
                    resources: false,
                    prompts: true,
                    directory_read: false,
                    logging: false,
                    experimental: HashMap::new(),
                    extensions: HashMap::new(),
                },
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: Vec::new(),
                resources: Vec::new(),
                resource_templates: Vec::new(),
                prompts: vec![McpPromptDto {
                    name: "draft".into(),
                    description: Some("draft prompt".into()),
                    arguments: Vec::new(),
                }],
                connected_at: SystemTime::now(),
            },
        );
        registry.clients.write().await.insert(
            "srv".into(),
            RegisteredClient {
                connection_id: Some(new_id),
                client: client_b,
            },
        );

        let error = registry
            .get_prompt(
                cached_id,
                "draft",
                serde_json::json!({ "topic": "release" }),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains(&format!(
            "MCP prompt connection {cached_id} is no longer active"
        )));
        assert!(
            peer_b.try_recv().is_err(),
            "the swapped L2 generation must not receive a stale prompts/get request",
        );
        drop(env);
    }

    #[tokio::test]
    async fn ensure_connected_client_accepts_raw_normalized_and_exact_scoped_names() {
        let registry = Arc::new(McpRegistry::new(Arc::new(BridgeMock::new(&[]))));
        let (raw_conn, _raw_peer) = observable_connection();
        let raw_client = Arc::new(
            McpClient::new("my.server", std::path::PathBuf::from("/tmp/work"), raw_conn).await,
        );
        registry
            .register_client("my.server", raw_client.clone())
            .await;

        let (claude_conn, _claude_peer) = observable_connection();
        let claude_client = Arc::new(
            McpClient::new(
                "claude.ai Linear",
                std::path::PathBuf::from("/tmp/work"),
                claude_conn,
            )
            .await,
        );
        registry
            .register_client("claude.ai Linear", claude_client.clone())
            .await;

        let scoped_key = "__lingxi_agent_scope__deadbeef__docs";
        let (scoped_conn, _scoped_peer) = observable_connection();
        let scoped_client = Arc::new(
            McpClient::new("docs", std::path::PathBuf::from("/tmp/work"), scoped_conn).await,
        );
        registry
            .register_client(scoped_key, scoped_client.clone())
            .await;

        let raw_by_raw = registry
            .ensure_connected_client("my.server")
            .await
            .expect("raw name lookup");
        let raw_by_normalized = registry
            .ensure_connected_client("my_server")
            .await
            .expect("normalized lookup");
        let claude_by_raw = registry
            .ensure_connected_client("claude.ai Linear")
            .await
            .expect("raw spaced name");
        let claude_by_normalized = registry
            .ensure_connected_client(&normalize_name_for_mcp("claude.ai Linear"))
            .await
            .expect("normalized spaced name");
        let scoped_by_exact = registry
            .ensure_connected_client(scoped_key)
            .await
            .expect("exact scoped key");

        assert!(Arc::ptr_eq(&raw_by_raw, &raw_client));
        assert!(Arc::ptr_eq(&raw_by_normalized, &raw_client));
        assert!(Arc::ptr_eq(&claude_by_raw, &claude_client));
        assert!(Arc::ptr_eq(&claude_by_normalized, &claude_client));
        assert!(Arc::ptr_eq(&scoped_by_exact, &scoped_client));
    }

    #[tokio::test]
    async fn connected_prompts_cache_generation_reaches_l1_twice_and_cleans_up_on_disconnect() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry_with_catalog(
            &store,
            &cache_key,
            0,
            ServerCapabilitiesDto {
                tools: false,
                resources: false,
                prompts: true,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            vec![],
            vec![],
            vec![McpPromptDto {
                name: "draft".into(),
                description: Some("draft prompt".into()),
                arguments: Vec::new(),
            }],
        );

        let prompt_mock = Arc::new(PromptBridgeMock::new());
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                prompt_mock.clone() as Arc<dyn McpTransport>,
                prompt_mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        let cached_id = registry
            .connect(cfg)
            .await
            .expect("fresh cache hit connect");
        let prompts = registry.connected_prompts().await;
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].0, "srv");
        assert_eq!(prompts[0].1, cached_id);
        assert_eq!(prompts[0].2.name, "draft");

        let responder = {
            let prompt_mock = prompt_mock.clone();
            tokio::spawn(async move {
                let connection_id = prompt_mock.wait_for_connection_id().await;
                prompt_mock.answer_next_prompt(connection_id).await;
                prompt_mock.answer_next_prompt(connection_id).await;
            })
        };

        let first = tokio::time::timeout(
            Duration::from_secs(2),
            registry.get_prompt(
                cached_id,
                "draft",
                serde_json::json!({ "topic": "release" }),
            ),
        )
        .await
        .expect("first cached prompt returns in time")
        .expect("first cached prompt lazy-dial succeeds");
        let second = tokio::time::timeout(
            Duration::from_secs(2),
            registry.get_prompt(
                cached_id,
                "draft",
                serde_json::json!({ "topic": "release" }),
            ),
        )
        .await
        .expect("second cached prompt returns in time")
        .expect("second cached prompt should keep routing C -> L1");
        responder.await.unwrap();
        registry
            .disconnect("srv")
            .await
            .expect("disconnect removes L1");
        let disconnected = registry
            .get_prompt(
                cached_id,
                "draft",
                serde_json::json!({ "topic": "release" }),
            )
            .await
            .unwrap_err();
        drop(env);

        let expected = serde_json::json!({
            "description": "Draft prompt",
            "messages": [{ "role": "user", "content": { "type": "text", "text": "topic=release" } }]
        });
        assert_eq!(
            prompt_mock.connect_calls.load(Ordering::SeqCst),
            1,
            "repeated cached prompt invocations must reuse the first live generation"
        );
        assert_eq!(first, expected);
        assert_eq!(second, expected);
        assert!(disconnected.to_string().contains(&format!(
            "MCP prompt connection {cached_id} is no longer active"
        )));
        assert!(
            !registry
                .prompt_predecessors
                .read()
                .await
                .contains_key(&cached_id),
            "disconnect must retire the cached prompt predecessor bridge"
        );
    }

    #[tokio::test]
    async fn stale_cached_prompt_command_survives_background_upgrade_before_first_invocation() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry_with_catalog(
            &store,
            &cache_key,
            1_000_000,
            ServerCapabilitiesDto {
                tools: false,
                resources: false,
                prompts: true,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            vec![],
            vec![],
            vec![McpPromptDto {
                name: "draft".into(),
                description: Some("draft prompt".into()),
                arguments: Vec::new(),
            }],
        );

        let prompt_mock = Arc::new(PromptBridgeMock::new());
        prompt_mock.block_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                prompt_mock.clone() as Arc<dyn McpTransport>,
                prompt_mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        let cached_id = registry
            .connect(cfg)
            .await
            .expect("stale cache hit connect");
        let prompts = registry.connected_prompts().await;
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].1, cached_id);

        prompt_mock.connect_started.notified().await;
        prompt_mock.block_connect.store(false, Ordering::SeqCst);
        prompt_mock.connect_release.notify_one();
        let live_id = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match registry.connections.read().await.get("srv") {
                    Some(McpConnectionState::Connected { connection_id, .. }) => {
                        break *connection_id;
                    }
                    _ => tokio::task::yield_now().await,
                }
            }
        })
        .await
        .expect("background upgrade publishes L1");

        let responder = {
            let prompt_mock = prompt_mock.clone();
            tokio::spawn(async move {
                let connection_id = prompt_mock.wait_for_connection_id().await;
                assert_eq!(connection_id, live_id);
                prompt_mock.answer_next_prompt(connection_id).await;
            })
        };
        let rendered = tokio::time::timeout(
            Duration::from_secs(2),
            registry.get_prompt(
                cached_id,
                "draft",
                serde_json::json!({ "topic": "release" }),
            ),
        )
        .await
        .expect("cached command returns after background publish")
        .expect("C -> L1 predecessor bridge remains valid");
        responder.await.unwrap();
        drop(env);

        assert_eq!(
            prompt_mock.connect_calls.load(Ordering::SeqCst),
            1,
            "using the cached prompt after a background upgrade must not dial L2"
        );
        assert_eq!(
            rendered,
            serde_json::json!({
                "description": "Draft prompt",
                "messages": [{ "role": "user", "content": { "type": "text", "text": "topic=release" } }]
            })
        );
    }

    #[tokio::test]
    async fn connected_state_is_not_visible_until_cached_prompt_predecessor_publishes() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry_with_catalog(
            &store,
            &cache_key,
            0,
            ServerCapabilitiesDto {
                tools: false,
                resources: false,
                prompts: true,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            vec![],
            vec![],
            vec![McpPromptDto {
                name: "draft".into(),
                description: Some("draft prompt".into()),
                arguments: Vec::new(),
            }],
        );

        let prompt_mock = Arc::new(PromptBridgeMock::new());
        prompt_mock.block_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                prompt_mock.clone() as Arc<dyn McpTransport>,
                prompt_mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );
        let cached_id = registry.connect(cfg).await.expect("fresh cache hit");

        let predecessor_guard = registry.prompt_predecessors.write().await;
        let waiter = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
        };
        let mut waiter = std::pin::pin!(waiter);
        prompt_mock.connect_started.notified().await;
        prompt_mock.block_connect.store(false, Ordering::SeqCst);
        prompt_mock.connect_release.notify_one();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), waiter.as_mut())
                .await
                .is_err(),
            "Connected(L1) must not publish before the cached prompt predecessor map can publish"
        );
        drop(predecessor_guard);

        let live_id = tokio::time::timeout(Duration::from_secs(2), waiter.as_mut())
            .await
            .expect("owner finishes after predecessor unlock")
            .expect("join succeeds")
            .expect("lazy dial succeeds");
        let responder = {
            let prompt_mock = prompt_mock.clone();
            tokio::spawn(async move {
                let connection_id = prompt_mock.wait_for_connection_id().await;
                assert_eq!(connection_id, live_id);
                prompt_mock.answer_next_prompt(connection_id).await;
            })
        };
        let rendered = registry
            .get_prompt(
                cached_id,
                "draft",
                serde_json::json!({ "topic": "release" }),
            )
            .await
            .expect("cached prompt C must be callable immediately once L1 is visible");
        responder.await.unwrap();
        drop(env);

        assert_eq!(
            rendered,
            serde_json::json!({
                "description": "Draft prompt",
                "messages": [{ "role": "user", "content": { "type": "text", "text": "topic=release" } }]
            })
        );
    }

    #[tokio::test]
    async fn connect_skips_client_when_raw_conn_none() {
        let mock = Arc::new(BridgeMock::new(&["read"]));
        // `new` wires NO RawConnectionProvider.
        let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
        registry.connect(cfg("mock")).await.unwrap();
        assert!(
            registry.get_client("mock").await.is_none(),
            "no client must be registered without a raw_conn bridge"
        );
    }

    #[tokio::test]
    async fn disconnect_drops_registered_client() {
        let mock = Arc::new(BridgeMock::new(&["read"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        );
        registry.connect(cfg("mock")).await.unwrap();
        assert!(registry.get_client("mock").await.is_some());
        registry.disconnect("mock").await.unwrap();
        assert!(
            registry.get_client("mock").await.is_none(),
            "disconnect must drop the cached client"
        );
    }

    // ---- Batch 2: FQN rewrite + normalize-match ----------------------------

    #[tokio::test]
    async fn connect_rewrites_fqn_with_normalized_server() {
        let mock = Arc::new(BridgeMock::new(&["read"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        );
        // Raw name `my.server` normalizes to `my_server` in the FQN, while the
        // stored key (and `server_name`) stay raw for `/mcp` display.
        registry.connect(cfg("my.server")).await.unwrap();

        let conns = registry.connections.read().await;
        let McpConnectionState::Connected { tools, .. } = conns.get("my.server").unwrap() else {
            panic!("expected Connected state");
        };
        assert_eq!(tools[0].full_name, "mcp__my_server__read");
        assert_eq!(tools[0].server_name, "my.server");
        drop(conns);

        // A model-supplied normalized `<server>` token resolves the raw key.
        assert!(registry.get_client("my_server").await.is_some());
        assert!(registry.get_config("my_server").await.is_some());
    }

    /// §26a — a server whose `initialize` declares the `resources` capability
    /// has its `resources/templates/list` fetched at connect time and stashed
    /// on the `Connected` state, exactly like `resources`/`tools`/`prompts`.
    /// Oracle gates `resources/templates/list` on the SAME capability as
    /// `resources/list` (@167690139), not a separate template bit — this is
    /// the connect-time "catalog fetch" the audit found entirely absent
    /// (registry.rs's fetch was `list_tools`/`list_resources`/`list_prompts`
    /// A stdio server must NOT be asked for `resources/templates/list`, even
    /// with the `resources` capability present.
    ///
    /// The oracle's live-discovery fetch (@182539595) is
    /// `v && JK(G.config) ? Qe(G) : Promise.resolve([])` — the `resources`
    /// capability AND cache eligibility. `JK` (@176260324) rejects any
    /// transport that is not `http`/`sse` outright, and the cache itself is
    /// off unless `MCP_DISCOVERY_CACHE` or the `tengu_mcp_discovery_cache_enable`
    /// gate says otherwise (both default off). So with stock settings the
    /// oracle issues ZERO of these RPCs, and never any for stdio.
    ///
    /// The assertion is the CALL COUNT, not the stored field: fetching and
    /// discarding would leave `resource_templates` empty too, and would still
    /// be the extra round trip this pins against.
    #[tokio::test]
    async fn connect_does_not_issue_the_templates_rpc_when_the_cache_is_ineligible() {
        let mock = Arc::new(BridgeMock::with_resource_templates(vec![
            platform_api::McpResourceTemplateDto {
                uri_template: "file:///{path}".into(),
                name: "file-template".into(),
                description: Some("A file on disk".into()),
                mime_type: Some("text/plain".into()),
                annotations: None,
                meta: None,
            },
        ]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        );
        // `cfg` builds a stdio spec, which `JK` rejects on transport alone.
        registry.connect(cfg("srv")).await.unwrap();

        assert_eq!(
            mock.templates_calls.load(Ordering::SeqCst),
            0,
            "a stdio server is cache-ineligible, so the oracle never issues \
             resources/templates/list for it — the port must not either"
        );
        let conns = registry.connections.read().await;
        let McpConnectionState::Connected {
            resource_templates, ..
        } = conns.get("srv").unwrap()
        else {
            panic!("expected Connected state");
        };
        assert!(
            resource_templates.is_empty(),
            "no fetch means no templates, got {resource_templates:?}"
        );
    }

    /// The capability gate: without the `resources` capability the fetch must
    /// be SKIPPED entirely (mirrors the existing `resources`/`prompts` gates
    /// just above it), not merely returning empty because the mock had none.
    #[tokio::test]
    async fn connect_skips_resource_templates_fetch_without_resources_capability() {
        // `BridgeMock::new` reports `resources: false` from `initialize`, so
        // even a mock stocked with templates must yield none on connect.
        let mut mock = BridgeMock::new(&[]);
        mock.resource_templates = vec![platform_api::McpResourceTemplateDto {
            uri_template: "file:///{path}".into(),
            name: "unreachable".into(),
            description: None,
            mime_type: None,
            annotations: None,
            meta: None,
        }];
        let mock = Arc::new(mock);
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        );
        registry.connect(cfg("srv")).await.unwrap();

        let conns = registry.connections.read().await;
        let McpConnectionState::Connected {
            resource_templates, ..
        } = conns.get("srv").unwrap()
        else {
            panic!("expected Connected state");
        };
        assert!(
            resource_templates.is_empty(),
            "resources capability absent -> templates fetch must be skipped, got {resource_templates:?}"
        );
    }

    /// §26a — a `resources/templates/list` FAILURE must never fail the
    /// connection. Templates are optional in the MCP spec: a server can
    /// legally declare `capabilities.resources` (because it registered
    /// `resources/list`) and answer `-32601 Method not found` for
    /// `resources/templates/list`. Oracle `Qe` (2.1.251 Mach-O @182528544)
    /// wraps the whole fetch in `try{...}catch(t){ ...; let r=[]; if(!(t
    /// instanceof Er&&t.code===Ir.MethodNotFound)) qt().discoveryFetchErrors
    /// .set(r,we(t)); return r }` — EVERY error path returns an empty array,
    /// and MethodNotFound is explicitly benign. Joining the fetch to the
    /// catalog block with `?` instead disconnected the live transport
    /// (registry.rs's `Err` arm) and returned `Err` from `connect`, so a
    /// server that connected fine before the fetch existed lost ALL of its
    /// A `-32601` on `resources/templates/list` must not fail the connection.
    ///
    /// This drives the ELIGIBLE path on purpose: an http spec with the
    /// discovery cache enabled is the only shape for which the oracle issues
    /// the RPC at all (`v && JK(G.config)`, @182539595), so it is the only
    /// shape under which this failure mode can arise. Gating the fetch made
    /// the previous stdio-based version of this test vacuous — the fetch
    /// never ran, so `list_resource_templates_fails` had nothing to fail.
    ///
    /// Oracle `Qe` @182528544 wraps the whole fetch in a catch that returns
    /// `[]` on EVERY error, so it can never fail a connection.
    #[tokio::test]
    async fn connect_survives_a_resource_templates_fetch_that_fails() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        mock.resources_capability.store(true, Ordering::SeqCst);
        mock.list_resource_templates_fails
            .store(true, Ordering::SeqCst);
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        );
        let connected = registry
            .connect(http_cfg("srv", "https://mcp.example.com/v1"))
            .await;
        drop(env);

        assert!(
            connected.is_ok(),
            "a -32601 on resources/templates/list must NOT fail the connection, got {:?}",
            connected.err()
        );
        assert_eq!(
            mock.templates_calls.load(Ordering::SeqCst),
            1,
            "the eligible path must actually issue the RPC, or this test proves nothing"
        );
        let conns = registry.connections.read().await;
        let McpConnectionState::Connected {
            tools,
            resource_templates,
            ..
        } = conns.get("srv").unwrap()
        else {
            panic!("expected Connected state");
        };
        assert_eq!(tools.len(), 1, "the server's tools must survive");
        assert!(
            resource_templates.is_empty(),
            "a failed template fetch yields an empty list, not an error"
        );
    }

    // ── §11 discovery-cache wiring ──────────────────────────────────────

    #[test]
    fn secret_refusal_extracts_composite_header_values() {
        let mut cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let McpTransportSpec::Http { headers, .. } = &mut cfg.spec else {
            unreachable!()
        };
        headers.insert(
            "Cookie".to_string(),
            "session=super-secret-value; theme=dark".to_string(),
        );

        let candidates = McpRegistry::config_secret_candidates(&cfg);
        assert!(candidates.iter().any(|value| value == "super-secret-value"));
        let serialized = serde_json::json!({"description": "super-secret-value"}).to_string();
        assert!(McpRegistry::discovery_cache_entry_reflects_secret(
            &serialized,
            &candidates
        ));
    }

    #[test]
    fn secret_refusal_detects_percent_encoded_token_values() {
        let candidates = vec!["tok+/=value?".to_string()];
        for reflected in ["tok%2B%2F%3Dvalue%3F", "tok%2b%2f%3dvalue%3f"] {
            let serialized = serde_json::json!({"description": reflected}).to_string();
            assert!(McpRegistry::discovery_cache_entry_reflects_secret(
                &serialized,
                &candidates
            ));
        }
    }

    #[tokio::test]
    async fn secret_refusal_checks_percent_encoded_stored_mcp_tokens() {
        let mut cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let McpTransportSpec::Http { oauth, .. } = &mut cfg.spec else {
            unreachable!()
        };
        *oauth = Some(platform_api::McpOAuthConfigDto {
            client_id: None,
            callback_port: None,
            auth_server_metadata_url: None,
            scopes: None,
            xaa: None,
        });

        let storage = Arc::new(XaaMemStorage::default());
        let storage_dyn = storage.clone() as Arc<dyn platform_api::SecureStorage>;
        let clock = Arc::new(FixedClock(std::time::UNIX_EPOCH)) as Arc<dyn platform_api::Clock>;
        let server_key = oauth::server_key(&cfg.name, &cfg.spec);
        oauth::store_tokens(
            &storage_dyn,
            &clock,
            &server_key,
            &oauth::StoredTokens {
                access_token: "access+/=token?".into(),
                refresh_token: Some("refresh+/=token?".into()),
                expires_at_unix: 1,
                client_id: None,
                client_secret: None,
                step_up_scope: None,
            },
        )
        .await
        .expect("store MCP tokens");

        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_oauth(OAuthDeps {
            http: GatedXaaHttp::new() as Arc<dyn platform_api::HttpTransport>,
            clock,
            storage: storage_dyn,
            on_authorization_url: Arc::new(|_| {}),
            xaa_config: None,
        });
        let candidates = registry
            .discovery_cache_secret_candidates_for(&cfg)
            .await
            .expect("secret candidates");

        for reflected in ["access%2B%2F%3Dtoken%3F", "refresh%2B%2F%3Dtoken%3F"] {
            let serialized = serde_json::json!({"description": reflected}).to_string();
            assert!(McpRegistry::discovery_cache_entry_reflects_secret(
                &serialized,
                &candidates
            ));
        }
    }

    #[tokio::test]
    async fn oauth_grant_provenance_writes_same_grant_and_rejects_rotation_for_each_scope() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        for (name, scope, source) in [
            (
                "shared-grant",
                ConfigScope::Settings(protocol::SettingsScope::User),
                None,
            ),
            (
                "agent-grant",
                ConfigScope::Agent,
                Some(crate::connection::McpAgentSource::BuiltIn),
            ),
        ] {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
            let mut cfg = http_cfg(name, "https://mcp.example.com/v1");
            cfg.scope = scope;
            cfg.metadata.agent_source = source;
            let McpTransportSpec::Http {
                oauth: oauth_config,
                ..
            } = &mut cfg.spec
            else {
                unreachable!()
            };
            *oauth_config = Some(platform_api::McpOAuthConfigDto {
                client_id: None,
                callback_port: None,
                auth_server_metadata_url: None,
                scopes: None,
                xaa: None,
            });

            let storage = Arc::new(XaaMemStorage::default());
            let storage_dyn = storage.clone() as Arc<dyn platform_api::SecureStorage>;
            let clock = Arc::new(FixedClock(std::time::UNIX_EPOCH)) as Arc<dyn platform_api::Clock>;
            let server_key = oauth::server_key(&cfg.name, &cfg.spec);
            let store_token = |access: &str, refresh: &str| {
                let storage = storage_dyn.clone();
                let clock = clock.clone();
                let server_key = server_key.clone();
                let access = access.to_string();
                let refresh = refresh.to_string();
                async move {
                    oauth::store_tokens(
                        &storage,
                        &clock,
                        &server_key,
                        &oauth::StoredTokens {
                            access_token: access,
                            refresh_token: Some(refresh),
                            expires_at_unix: u64::MAX,
                            client_id: None,
                            client_secret: None,
                            step_up_scope: None,
                        },
                    )
                    .await
                    .expect("store test grant");
                }
            };
            store_token("access-a", "refresh-a").await;

            let mock = Arc::new(BridgeMock::new(&[]));
            let registry = McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_oauth(OAuthDeps {
                http: GatedXaaHttp::new() as Arc<dyn platform_api::HttpTransport>,
                clock: clock.clone(),
                storage: storage_dyn.clone(),
                on_authorization_url: Arc::new(|_| {}),
                xaa_config: None,
            })
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            );
            let grant = registry
                .current_grant_provenance(&cfg)
                .await
                .expect("current grant")
                .expect("OAuth storage grant");
            let partition = registry
                .discovery_cache_partition_for_grant(
                    &cfg,
                    crate::protocol_negotiation::NegotiationMode::Legacy,
                    Some(&grant),
                )
                .await
                .expect("grant partition");
            let caps = ServerCapabilitiesDto {
                tools: true,
                ..ServerCapabilitiesDto::default()
            };
            let protocol = platform_api::McpNegotiatedProtocol {
                era: platform_api::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            };
            let tool = |name: &str| McpToolDto {
                server_name: cfg.name.clone(),
                tool_name: name.into(),
                description: name.into(),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                annotations: None,
                icons: Vec::new(),
                meta: None,
                full_name: format!("mcp__{}__{name}", cfg.name),
                search_hint: None,
                always_load: None,
                requires_user_interaction: false,
            };
            registry
                .persist_or_purge_discovery_cache(
                    &cfg,
                    Some(&partition),
                    &caps,
                    &[tool("alpha")],
                    &[],
                    &[],
                    &[],
                    crate::protocol_negotiation::NegotiationMode::Legacy,
                    Some(&grant),
                    Some(&protocol),
                )
                .await;
            let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
            let saved = store.load_partitioned(&cache_key, &partition.partition_key);
            assert!(matches!(
                saved,
                crate::discovery_cache::EntryLookup::Found(_)
            ));

            store_token("access-b", "refresh-b").await;
            registry
                .persist_or_purge_discovery_cache(
                    &cfg,
                    Some(&partition),
                    &caps,
                    &[tool("beta")],
                    &[],
                    &[],
                    &[],
                    crate::protocol_negotiation::NegotiationMode::Legacy,
                    Some(&grant),
                    Some(&protocol),
                )
                .await;
            let saved = store.load_partitioned(&cache_key, &partition.partition_key);
            assert!(matches!(
                saved,
                crate::discovery_cache::EntryLookup::Found(entry)
                    if entry.tools.iter().any(|tool| tool.tool_name == "alpha")
                        && entry.tools.iter().all(|tool| tool.tool_name != "beta")
            ));
            let rotated = registry
                .current_grant_provenance(&cfg)
                .await
                .expect("rotated grant")
                .expect("rotated OAuth storage grant");
            let rotated_partition = registry
                .discovery_cache_partition_for_grant(
                    &cfg,
                    crate::protocol_negotiation::NegotiationMode::Legacy,
                    Some(&rotated),
                )
                .await
                .expect("rotated partition");
            assert_ne!(partition.partition_key, rotated_partition.partition_key);
            assert!(matches!(
                store.load_partitioned(&cache_key, &rotated_partition.partition_key),
                crate::discovery_cache::EntryLookup::Absent
            ));
        }
        drop(env);
    }

    #[tokio::test]
    async fn oauth_without_refresh_grant_is_not_partition_or_write_eligible() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let mut cfg = http_cfg("access-only", "https://mcp.example.com/v1");
        let McpTransportSpec::Http {
            oauth: oauth_config,
            ..
        } = &mut cfg.spec
        else {
            unreachable!()
        };
        *oauth_config = Some(platform_api::McpOAuthConfigDto {
            client_id: None,
            callback_port: None,
            auth_server_metadata_url: None,
            scopes: None,
            xaa: None,
        });
        let storage = Arc::new(XaaMemStorage::default());
        let storage_dyn = storage.clone() as Arc<dyn platform_api::SecureStorage>;
        let clock = Arc::new(FixedClock(std::time::UNIX_EPOCH)) as Arc<dyn platform_api::Clock>;
        let key = oauth::server_key(&cfg.name, &cfg.spec);
        oauth::store_tokens(
            &storage_dyn,
            &clock,
            &key,
            &oauth::StoredTokens {
                access_token: "access-only".into(),
                refresh_token: None,
                expires_at_unix: u64::MAX,
                client_id: None,
                client_secret: None,
                step_up_scope: None,
            },
        )
        .await
        .expect("store access-only token");
        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_oauth(OAuthDeps {
            http: GatedXaaHttp::new() as Arc<dyn platform_api::HttpTransport>,
            clock,
            storage: storage_dyn,
            on_authorization_url: Arc::new(|_| {}),
            xaa_config: None,
        })
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        assert!(registry
            .current_grant_provenance(&cfg)
            .await
            .expect("current grant")
            .is_none());
        assert!(matches!(
            registry
                .discovery_cache_partition_for(
                    &cfg,
                    crate::protocol_negotiation::NegotiationMode::Legacy,
                )
                .await,
            Err(crate::discovery_cache::MissReason::NoFingerprint)
        ));
        registry
            .persist_or_purge_discovery_cache(
                &cfg,
                None,
                &ServerCapabilitiesDto::default(),
                &[],
                &[],
                &[],
                &[],
                crate::protocol_negotiation::NegotiationMode::Legacy,
                None,
                None,
            )
            .await;
        assert!(
            std::fs::read_dir(dir.path())
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(true),
            "access-only OAuth must never create a discovery-cache partition"
        );
        drop(env);
    }

    /// A `None` `discovery_cache_store` (every registry not built with
    /// [`McpRegistry::with_discovery_cache_store`]) must leave every §11
    /// helper a total no-op: no store, no write, no purge, no strike, no
    /// telemetry decision. This is the guard that keeps every EXISTING
    /// connect test in this file (none of which wires a store) unaffected.
    #[tokio::test]
    async fn no_store_configured_is_a_total_no_op() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        );
        let result = registry
            .connect(http_cfg("srv", "https://mcp.example.com/v1"))
            .await;
        drop(env);

        assert!(result.is_ok(), "no store must never perturb a connect");
    }

    /// The write half: a successful live discovery for a cache-ELIGIBLE
    /// server (http, feature enabled, no headers helper) must persist the
    /// FULL catalog to disk, versioned, with strikes reset to 0. Asserted
    /// against the store's own `load`, not the `Connected` state — proving
    /// the SEPARATE persistence path actually ran.
    #[tokio::test]
    async fn connect_persists_a_discovery_cache_entry_for_an_eligible_server() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        registry.connect(cfg).await.unwrap();
        drop(env);

        let entry = match load_test_entry(&store, &cache_key) {
            crate::discovery_cache::EntryLookup::Found(entry) => entry,
            other => panic!("expected a persisted entry, got {other:?}"),
        };
        assert_eq!(entry.version, crate::discovery_cache::CACHE_SCHEMA_VERSION);
        assert_eq!(entry.cache_key, cache_key);
        assert_eq!(entry.consecutive_refresh_failures, 0);
        assert!(
            entry.capabilities.tools,
            "the mock declares the tools capability"
        );
        assert_eq!(
            entry
                .tools
                .iter()
                .map(|t| t.tool_name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha"],
            "the persisted entry must carry the ACTUAL discovered catalog"
        );
    }

    /// A gate-ineligible server (stdio: [`crate::discovery_cache::CacheGateReason::Transport`])
    /// must never touch the store at all, even with a store wired and the
    /// feature enabled.
    #[tokio::test]
    async fn connect_does_not_persist_for_a_transport_ineligible_server() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        // `cfg` builds a stdio spec — `Transport`-ineligible regardless of
        // the feature flag.
        registry.connect(cfg("srv")).await.unwrap();
        drop(env);

        assert!(
            std::fs::read_dir(dir.path())
                .map(|mut it| it.next().is_none())
                .unwrap_or(true),
            "a stdio (transport-ineligible) connect must create NO cache files at all"
        );
    }

    /// The purge half: [`McpRegistry::persist_or_purge_discovery_cache`]
    /// must remove any EXISTING on-disk entry when the gate reason is
    /// [`crate::discovery_cache::CacheGateReason::HeadersHelper`] — the one
    /// gate reason besides `OptOut` that
    /// [`crate::discovery_cache::CacheGateReason::purges_existing_entry`]
    /// flags. Called directly (not through `connect`) because a real
    /// `headersHelper` would spawn an actual subprocess —
    /// the gate decision itself does not depend on that subprocess ever
    /// running, only on `config.spec` carrying `headers_helper: Some(_)`.
    #[tokio::test]
    async fn persist_or_purge_removes_an_existing_entry_when_the_gate_is_headers_helper() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        let mut cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let McpTransportSpec::Http { headers_helper, .. } = &mut cfg.spec else {
            unreachable!()
        };
        *headers_helper = Some("./helper".to_string());
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);

        // Pre-seed an entry as if it were written before `headersHelper` got
        // configured on this server.
        store_test_entry(
            &store,
            &crate::discovery_cache::DiscoveryCacheEntry::new(
                cache_key.clone(),
                1,
                ServerCapabilitiesDto::default(),
                vec![],
                vec![],
                vec![],
                vec![],
            ),
        );
        assert!(matches!(
            load_test_entry(&store, &cache_key),
            crate::discovery_cache::EntryLookup::Found(_)
        ));

        registry
            .persist_or_purge_discovery_cache(
                &cfg,
                None,
                &ServerCapabilitiesDto::default(),
                &[],
                &[],
                &[],
                &[],
                crate::protocol_negotiation::NegotiationMode::Legacy,
                None,
                None,
            )
            .await;
        drop(env);

        assert_eq!(
            load_test_entry(&store, &cache_key),
            crate::discovery_cache::EntryLookup::Absent,
            "a headersHelper-gated server must have its stale entry purged"
        );
    }

    #[tokio::test]
    async fn provenance_gates_skip_cache_read_and_purge() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let mut cli_owned = http_cfg("cli-owned", "https://mcp.example.com/v1");
        cli_owned.metadata.cli_owned = true;
        let env_placeholder = http_cfg("env-placeholder", "https://${MCP_HOST}/v1");
        let mut ambient_credential = http_cfg("ambient-credential", "https://mcp.example.com/v1");
        ambient_credential.metadata.ambient_credential = true;
        let mut agent_without_source =
            http_cfg("agent-without-source", "https://mcp.example.com/v1");
        agent_without_source.scope = ConfigScope::Agent;
        let scenarios = [
            (cli_owned, crate::discovery_cache::MissReason::CliOwned),
            (
                env_placeholder,
                crate::discovery_cache::MissReason::EnvPlaceholder,
            ),
            (
                ambient_credential,
                crate::discovery_cache::MissReason::AmbientCredential,
            ),
            (
                agent_without_source,
                crate::discovery_cache::MissReason::NoFingerprint,
            ),
        ];

        for (config, expected_reason) in scenarios {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
            let cache_key = crate::discovery_cache::logical_cache_key(&config);
            store_test_entry(
                &store,
                &crate::discovery_cache::DiscoveryCacheEntry::new(
                    cache_key.clone(),
                    1,
                    ServerCapabilitiesDto::default(),
                    vec![],
                    vec![],
                    vec![],
                    vec![],
                ),
            );
            let mock = Arc::new(BridgeMock::new(&[]));
            let registry = McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            );

            let consult = registry
                .discovery_cache_decision_for(
                    &config,
                    crate::protocol_negotiation::NegotiationMode::Legacy,
                )
                .await
                .expect("store is configured");
            assert_eq!(
                consult.decision,
                crate::discovery_cache::Decision::Miss {
                    reason: expected_reason
                },
                "provenance gate must short-circuit before an on-disk lookup"
            );
            assert!(
                consult.partition.is_none(),
                "provenance gate must not resolve a cache partition"
            );
            assert!(matches!(
                load_test_entry(&store, &cache_key),
                crate::discovery_cache::EntryLookup::Found(_)
            ));
        }
        drop(env);
    }

    /// A real parsed/runtime `discoveryCache:false` value must purge the
    /// server's existing cache family, dial live, and decline write-through.
    #[tokio::test]
    async fn discovery_cache_false_purges_then_dials_without_rewriting() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let mut cfg = http_cfg("srv", "https://mcp.example.com/v1");
        cfg.discovery_cache = Some(false);
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        store_test_entry(
            &store,
            &crate::discovery_cache::DiscoveryCacheEntry::new(
                cache_key.clone(),
                1,
                ServerCapabilitiesDto::default(),
                vec![],
                vec![],
                vec![],
                vec![],
            ),
        );

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        registry.connect(cfg).await.expect("live opt-out connect");
        drop(env);

        assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            load_test_entry(&store, &cache_key),
            crate::discovery_cache::EntryLookup::Absent,
            "opt-out must purge the old partition and must not write a new one"
        );
    }

    /// Oracle `_6e` strikes belong only to stale background revalidation that
    /// actually fails the connection. A catalog failure after successful
    /// initialize must not strike an existing cache entry.
    #[tokio::test]
    async fn an_ordinary_partial_connect_does_not_strike_an_existing_entry() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        store_test_entry(
            &store,
            &crate::discovery_cache::DiscoveryCacheEntry::new(
                cache_key.clone(),
                1,
                ServerCapabilitiesDto::default(),
                vec![],
                vec![],
                vec![],
                vec![],
            ),
        );

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        mock.list_tools_fails.store(true, Ordering::SeqCst);
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        let connection_id = registry
            .connect(cfg)
            .await
            .expect("transport initialize succeeds despite tools/list failure");
        drop(env);

        assert!(matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Connected { connection_id: current, tools, .. })
                if *current == connection_id && tools.is_empty()
        ));
        let entry = match load_test_entry(&store, &cache_key) {
            crate::discovery_cache::EntryLookup::Found(entry) => entry,
            other => panic!("expected the seeded entry to survive, got {other:?}"),
        };
        assert_eq!(
            entry.consecutive_refresh_failures, 0,
            "ordinary partial connect must not record a stale-refresh strike"
        );
    }

    #[test]
    fn stale_refresh_strikes_only_the_partition_that_served_the_hit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let logical_key = crate::discovery_cache::logical_cache_key(&cfg);
        let old_partition = DiscoveryCachePartition {
            logical_key: logical_key.clone(),
            partition_key: crate::discovery_cache::partition_key(
                &logical_key,
                &crate::discovery_cache::fingerprint("grant:old"),
            ),
            expected_era: "legacy",
            negotiation_mode: crate::protocol_negotiation::NegotiationMode::Legacy,
        };
        let new_partition = DiscoveryCachePartition {
            logical_key: logical_key.clone(),
            partition_key: crate::discovery_cache::partition_key(
                &logical_key,
                &crate::discovery_cache::fingerprint("grant:new"),
            ),
            expected_era: "legacy",
            negotiation_mode: crate::protocol_negotiation::NegotiationMode::Legacy,
        };
        let entry = crate::discovery_cache::DiscoveryCacheEntry::new(
            logical_key.clone(),
            1,
            ServerCapabilitiesDto::default(),
            vec![],
            vec![],
            vec![],
            vec![],
        );
        store
            .store_partitioned(&entry, &old_partition.partition_key)
            .expect("seed old partition");
        store
            .store_partitioned(&entry, &new_partition.partition_key)
            .expect("seed rotated partition");

        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        registry.record_discovery_cache_refresh_failure(&cfg, &old_partition);

        let old = match store.load_partitioned(&logical_key, &old_partition.partition_key) {
            crate::discovery_cache::EntryLookup::Found(entry) => entry,
            other => panic!("expected old partition, got {other:?}"),
        };
        let new = match store.load_partitioned(&logical_key, &new_partition.partition_key) {
            crate::discovery_cache::EntryLookup::Found(entry) => entry,
            other => panic!("expected rotated partition, got {other:?}"),
        };
        assert_eq!(old.consecutive_refresh_failures, 1);
        assert_eq!(
            new.consecutive_refresh_failures, 0,
            "a refresh-token rotation must not move the strike to the new partition"
        );
    }

    #[tokio::test]
    async fn stale_refresh_era_change_purges_hit_partition_without_replacement_or_strike() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let logical_key = crate::discovery_cache::logical_cache_key(&cfg);
        let partition = DiscoveryCachePartition {
            logical_key: logical_key.clone(),
            partition_key: crate::discovery_cache::partition_key(
                &logical_key,
                &crate::discovery_cache::fingerprint("grant:none"),
            ),
            expected_era: "legacy",
            negotiation_mode: crate::protocol_negotiation::NegotiationMode::Legacy,
        };
        let entry = crate::discovery_cache::DiscoveryCacheEntry::new(
            logical_key.clone(),
            1,
            ServerCapabilitiesDto::default(),
            vec![],
            vec![],
            vec![],
            vec![],
        );
        store
            .store_partitioned(&entry, &partition.partition_key)
            .expect("seed stale partition");
        let other_partition_key = crate::discovery_cache::partition_key(
            &logical_key,
            &crate::discovery_cache::fingerprint("grant:other"),
        );
        store
            .store_partitioned(&entry, &other_partition_key)
            .expect("seed other partition");

        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));
        let cached_connection_id = registry
            .serve_discovery_cache_hit(&cfg, "srv", entry, 1_000_000, false, None)
            .await
            .expect("cache hit");
        let slot = Arc::new(LazyUpgradeSlot::new(
            "srv".into(),
            cached_connection_id,
            cfg.clone(),
            Some(partition.clone()),
            Some("legacy".into()),
            crate::protocol_negotiation::NegotiationMode::Legacy,
            LazyUpgradeMode::Background,
        ));
        registry
            .lazy_upgrade_slots
            .write()
            .await
            .insert("srv".into(), slot.clone());

        let discovery = LiveDiscovery {
            connection_id: McpConnectionId::new(),
            connection_duration_ms: 1,
            grant_provenance: Some(GrantProvenance::unbound()),
            negotiation_mode: crate::protocol_negotiation::NegotiationMode::Legacy,
            negotiated: platform_api::McpNegotiatedProtocol {
                era: platform_api::McpProtocolEra::Modern,
                version: "2026-07-28".into(),
            },
            capabilities: ServerCapabilitiesDto::default(),
            tools: vec![],
            resources: vec![],
            resource_templates: vec![],
            prompts: vec![],
            catalog_failures: CatalogFetchFailures::default(),
            discovery_cache_partition: Some(partition.clone()),
            client: None,
            listener_connection: None,
        };
        assert!(matches!(
            registry
                .install_lazy_upgrade_live_discovery_if_current("srv", &slot, discovery)
                .await,
            BackgroundInstallOutcome::Rejected(_)
        ));
        assert!(matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Cached { connection_id, .. })
                if *connection_id == cached_connection_id
        ));
        assert_eq!(
            store.load_partitioned(&logical_key, &partition.partition_key),
            crate::discovery_cache::EntryLookup::Absent,
            "an era change retires only the partition that served the stale hit"
        );
        assert!(matches!(
            store.load_partitioned(&logical_key, &other_partition_key),
            crate::discovery_cache::EntryLookup::Found(entry)
                if entry.consecutive_refresh_failures == 0
        ));
    }

    #[tokio::test]
    async fn stale_refresh_expected_mode_change_purges_without_publish_strike_or_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let logical_key = crate::discovery_cache::logical_cache_key(&cfg);
        let partition = DiscoveryCachePartition {
            logical_key: logical_key.clone(),
            partition_key: crate::discovery_cache::partition_key(
                &logical_key,
                &crate::discovery_cache::fingerprint("grant:none"),
            ),
            expected_era: "legacy",
            negotiation_mode: crate::protocol_negotiation::NegotiationMode::Legacy,
        };
        let entry = crate::discovery_cache::DiscoveryCacheEntry::new(
            logical_key.clone(),
            1,
            ServerCapabilitiesDto::default(),
            vec![],
            vec![],
            vec![],
            vec![],
        );
        store
            .store_partitioned(&entry, &partition.partition_key)
            .expect("seed stale partition");

        let mock = Arc::new(BridgeMock::new(&[]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));
        let cached_connection_id = registry
            .serve_discovery_cache_hit(&cfg, "srv", entry, 1_000_000, false, None)
            .await
            .expect("cache hit");
        let slot = Arc::new(LazyUpgradeSlot::new(
            "srv".into(),
            cached_connection_id,
            cfg.clone(),
            Some(partition.clone()),
            Some("legacy".into()),
            crate::protocol_negotiation::NegotiationMode::Legacy,
            LazyUpgradeMode::Background,
        ));
        registry
            .lazy_upgrade_slots
            .write()
            .await
            .insert("srv".into(), slot.clone());

        let discovery = LiveDiscovery {
            connection_id: McpConnectionId::new(),
            connection_duration_ms: 1,
            grant_provenance: Some(GrantProvenance::unbound()),
            // The live handshake actually stayed legacy, but its immutable
            // resolver mode changed. That is enough to reject the stale hit.
            negotiation_mode: crate::protocol_negotiation::NegotiationMode::Auto {
                probe_timeout_ms: 1_000,
            },
            negotiated: platform_api::McpNegotiatedProtocol {
                era: platform_api::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            capabilities: ServerCapabilitiesDto::default(),
            tools: vec![],
            resources: vec![],
            resource_templates: vec![],
            prompts: vec![],
            catalog_failures: CatalogFetchFailures::default(),
            discovery_cache_partition: Some(partition.clone()),
            client: None,
            listener_connection: None,
        };
        assert!(matches!(
            registry
                .install_lazy_upgrade_live_discovery_if_current("srv", &slot, discovery)
                .await,
            BackgroundInstallOutcome::Rejected(_)
        ));
        assert!(matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Cached { connection_id, .. })
                if *connection_id == cached_connection_id
        ));
        assert_eq!(
            store.load_partitioned(&logical_key, &partition.partition_key),
            crate::discovery_cache::EntryLookup::Absent,
            "expected mode drift purges the partition that served the stale hit"
        );
        assert_eq!(mock.connect_calls.load(Ordering::SeqCst), 0);
    }

    /// A server with no prior cache entry still persists the connected
    /// generation when transport initialize succeeds and only `tools/list`
    /// fails. The stored entry must remain strike-free and carry the empty
    /// degraded tools slice.
    #[tokio::test]
    async fn a_partial_connect_with_no_existing_entry_persists_a_strike_free_empty_catalog() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        mock.list_tools_fails.store(true, Ordering::SeqCst);
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        let connection_id = registry.connect(cfg.clone()).await;
        let negotiation_mode = crate::protocol_negotiation::resolve_for_spec_with_transport(
            &cfg.spec,
            cfg.metadata.transport.as_deref(),
            mcp_connection_timeout().as_millis() as u64,
        );
        let partition = registry
            .discovery_cache_partition_for(&cfg, negotiation_mode)
            .await
            .expect("eligible config gets a partition");
        drop(env);

        let connection_id = connection_id.expect("partial connect still succeeds");
        assert!(matches!(
            registry.connections.read().await.get("srv"),
            Some(McpConnectionState::Connected { connection_id: current, tools, .. })
                if *current == connection_id && tools.is_empty()
        ));
        let entry = match store.load_partitioned(&cache_key, &partition.partition_key) {
            crate::discovery_cache::EntryLookup::Found(entry) => entry,
            other => panic!("expected a newly persisted entry, got {other:?}"),
        };
        assert_eq!(entry.consecutive_refresh_failures, 0);
        assert!(
            entry.tools.is_empty(),
            "failed tools/list persists the degraded empty tools slice on first connect"
        );
    }

    // ── §11 Stage 2: serve discovery-cache hits without dialing, lazy dial ──

    /// Seed a discovery-cache entry for `srv`, saved `age_ms` in the past
    /// (relative to "now"), for the Stage 2/3 cache tests below. `age_ms`
    /// alone decides Fresh (< 900s default TTL) vs Stale (>= TTL,
    /// < 14 400s default max-stale).
    fn seed_entry_with_catalog(
        store: &crate::discovery_cache::DiscoveryCacheStore,
        cache_key: &str,
        age_ms: u64,
        capabilities: ServerCapabilitiesDto,
        tools: Vec<McpToolDto>,
        resources: Vec<McpResourceDto>,
        prompts: Vec<McpPromptDto>,
    ) {
        let saved_at_ms = crate::discovery_cache::now_ms().saturating_sub(age_ms);
        store_test_entry(
            store,
            &crate::discovery_cache::DiscoveryCacheEntry::new(
                cache_key.to_string(),
                saved_at_ms,
                capabilities,
                tools,
                resources,
                vec![],
                prompts,
            ),
        );
    }

    fn default_test_partition_key(cache_key: &str) -> String {
        let fingerprint = crate::discovery_cache::fingerprint("grant:none");
        crate::discovery_cache::partition_key(cache_key, &fingerprint)
    }

    fn store_test_entry(
        store: &crate::discovery_cache::DiscoveryCacheStore,
        entry: &crate::discovery_cache::DiscoveryCacheEntry,
    ) {
        store
            .store_partitioned(entry, &default_test_partition_key(&entry.cache_key))
            .expect("seed partitioned store");
    }

    fn load_test_entry(
        store: &crate::discovery_cache::DiscoveryCacheStore,
        cache_key: &str,
    ) -> crate::discovery_cache::EntryLookup {
        store.load_partitioned(cache_key, &default_test_partition_key(cache_key))
    }

    fn seed_entry(
        store: &crate::discovery_cache::DiscoveryCacheStore,
        cache_key: &str,
        age_ms: u64,
    ) {
        seed_entry_with_catalog(
            store,
            cache_key,
            age_ms,
            ServerCapabilitiesDto {
                tools: true,
                resources: false,
                prompts: false,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            vec![McpToolDto {
                server_name: "srv".into(),
                tool_name: "alpha".into(),
                description: "alpha tool".into(),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                annotations: None,
                icons: Vec::new(),
                meta: None,
                full_name: "mcp__srv__alpha".into(),
                search_hint: None,
                always_load: None,
                requires_user_interaction: false,
            }],
            vec![],
            vec![],
        );
    }

    /// The headline Stage 2 claim: a `Fresh` cache hit serves the cached
    /// catalog and dials the transport ZERO times. Proven with the
    /// TRANSPORT CALL COUNT (`mock.connect_calls`) — not merely "tools are
    /// present", which a "dial-then-discard" bug would also satisfy.
    #[tokio::test]
    async fn a_fresh_cache_hit_serves_without_dialing() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0); // saved "now" — well inside the TTL.

        // The mock's OWN tools deliberately differ from the cached ones, so a
        // test that accidentally dialed live would be caught by tool identity
        // too, not just the call count.
        let mock = Arc::new(BridgeMock::new(&["should_never_be_dialed"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        let result = registry.connect(cfg).await;
        drop(env);

        assert!(result.is_ok(), "a cache hit must succeed, got {result:?}");
        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            0,
            "a Fresh cache hit must dial the transport ZERO times"
        );
        {
            let conns = registry.connections.read().await;
            let McpConnectionState::Cached {
                tools,
                cache_saved_at_ms,
                ..
            } = conns.get("srv").unwrap()
            else {
                panic!("expected Cached state, got {:?}", conns.get("srv"));
            };
            assert_eq!(
                tools
                    .iter()
                    .map(|t| t.tool_name.as_str())
                    .collect::<Vec<_>>(),
                vec!["alpha"],
                "the served catalog must be the CACHED one, not the mock's live tools"
            );
            assert!(*cache_saved_at_ms > 0);
        }
        assert!(
            registry.has_callable_server("srv").await,
            "a Cached server must report callable"
        );
    }

    /// A `Stale` hit (past the 900s TTL but inside the 14 400s max-stale
    /// window) must return the cached generation immediately, without waiting
    /// for the Stage 3 background revalidation dial to finish.
    #[tokio::test]
    async fn a_stale_cache_hit_also_serves_without_dialing() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        // 1_000_000ms (~16.7min) > the 900_000ms default TTL, but well under
        // the 14_400_000ms default max-stale.
        seed_entry(&store, &cache_key, 1_000_000);

        let mock = Arc::new(BridgeMock::new(&["fresh_live"]));
        mock.block_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        let connect = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.connect(cfg).await })
        };
        let cached_id = tokio::time::timeout(Duration::from_millis(200), connect)
            .await
            .expect("stale hit must return before background dial finishes")
            .expect("join")
            .expect("stale hit succeeds");
        mock.connect_started.notified().await;

        let conns = registry.connections.read().await;
        assert!(
            matches!(
                conns.get("srv"),
                Some(McpConnectionState::Cached { connection_id, .. }) if *connection_id == cached_id
            ),
            "expected Cached state, got {:?}",
            conns.get("srv")
        );
        drop(conns);

        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    registry.connections.read().await.get("srv"),
                    Some(McpConnectionState::Connected { .. })
                ) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background revalidation completes");
        drop(env);
    }

    #[tokio::test]
    async fn a_stale_cache_hit_returns_immediately_and_revalidates_in_background() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 1_000_000);

        let mock = Arc::new(BridgeMock::new(&["fresh_live"]));
        mock.block_connect.store(true, Ordering::SeqCst);
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        let cached_id = registry
            .connect(cfg)
            .await
            .expect("stale cache hit connect");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if mock.connect_calls.load(Ordering::SeqCst) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background revalidation must start");
        {
            let conns = registry.connections.read().await;
            assert!(
                matches!(
                    conns.get("srv"),
                    Some(McpConnectionState::Cached { connection_id, .. }) if *connection_id == cached_id
                ),
                "the stale-hit connect must return the cached generation without waiting"
            );
        }

        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    registry.connections.read().await.get("srv"),
                    Some(McpConnectionState::Connected { .. })
                ) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background revalidation completes");
        drop(env);

        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            1,
            "the stale hit must trigger exactly one live revalidation dial"
        );
        let entry = match load_test_entry(&store, &cache_key) {
            crate::discovery_cache::EntryLookup::Found(entry) => entry,
            other => panic!("expected refreshed entry, got {other:?}"),
        };
        assert_eq!(
            entry.consecutive_refresh_failures, 0,
            "a successful background refresh must clear strikes"
        );
        assert_eq!(
            entry
                .tools
                .iter()
                .map(|tool| tool.tool_name.as_str())
                .collect::<Vec<_>>(),
            vec!["fresh_live"],
            "the background refresh must atomically replace the cached catalog"
        );
    }

    #[tokio::test]
    async fn a_stale_cache_hit_background_tools_failure_preserves_cached_tools_without_a_strike() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 1_000_000);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        mock.list_tools_fails.store(true, Ordering::SeqCst);
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        let cached_id = registry
            .connect(cfg)
            .await
            .expect("stale cache hit connect");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let entry = match load_test_entry(&store, &cache_key) {
                    crate::discovery_cache::EntryLookup::Found(entry) => entry,
                    other => panic!("expected seeded entry, got {other:?}"),
                };
                let upgraded = matches!(
                    registry.connections.read().await.get("srv"),
                    Some(McpConnectionState::Connected { connection_id, tools, .. })
                        if *connection_id != cached_id
                            && tools.iter().any(|tool| tool.tool_name == "alpha")
                );
                if upgraded && entry.consecutive_refresh_failures == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background partial refresh installs a live connection");
        drop(env);

        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            1,
            "the background partial refresh must still attempt one dial"
        );
        let entry = match load_test_entry(&store, &cache_key) {
            crate::discovery_cache::EntryLookup::Found(entry) => entry,
            other => panic!("expected refreshed cache entry, got {other:?}"),
        };
        assert_eq!(entry.consecutive_refresh_failures, 0);
        assert_eq!(
            entry
                .tools
                .iter()
                .map(|tool| tool.tool_name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha"],
            "previous-safe tools must be preserved in the refreshed cache entry"
        );
    }

    #[tokio::test]
    async fn background_revalidation_owner_is_shared_with_a_foreground_waiter() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 1_000_000);

        let mock = Arc::new(BridgeMock::new(&["fresh_live"]));
        mock.block_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        let cached_id = registry.connect(cfg).await.expect("stale hit connect");
        mock.connect_started.notified().await;
        let waiter = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            matches!(
                registry.connections.read().await.get("srv"),
                Some(McpConnectionState::Cached { connection_id, .. }) if *connection_id == cached_id
            ),
            "background-first owner must keep the cached generation visible while foreground joins"
        );
        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            1,
            "foreground join must not open a second transport when background already owns the slot"
        );

        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        let live_id = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("foreground waiter finishes")
            .expect("waiter join succeeds")
            .expect("foreground waiter receives L1");
        drop(env);

        assert_ne!(live_id, cached_id);
        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            1,
            "background and foreground must share one exact-key owner"
        );
    }

    #[tokio::test]
    async fn background_start_detects_an_existing_foreground_owner_without_redialing() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        mock.block_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        registry.connect(cfg).await.expect("fresh cache hit");
        let waiter = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
        };
        mock.connect_started.notified().await;
        let lifecycle = registry.lifecycle_lock("srv");
        let _guard = lifecycle.lock().await;
        let prepare = registry
            .prepare_lazy_upgrade_slot_locked(
                "srv",
                LazyUpgradeMode::Background,
                None,
                None,
                crate::protocol_negotiation::NegotiationMode::Legacy,
            )
            .await
            .expect("background probe");
        drop(_guard);
        assert!(
            matches!(prepare, LazyUpgradePreparation::Wait(_, false)),
            "background startup must detect the existing foreground owner and refrain from spawning another dial"
        );
        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            1,
            "existing foreground owner already holds the only live dial"
        );

        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        waiter.await.unwrap().expect("foreground owner completes");
        drop(env);
    }

    #[tokio::test]
    async fn foreground_lazy_upgrade_panic_recovers_to_disconnected_and_unblocks_waiters() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        mock.panic_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        registry.connect(cfg).await.expect("cache hit connect");
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            registry.ensure_dialed_from_cache("srv"),
        )
        .await
        .expect("waiter completes after panic")
        .unwrap_err();
        drop(env);

        assert!(
            error
                .to_string()
                .contains("cached lazy-upgrade task panicked"),
            "panic recovery must surface a terminal waiter error"
        );
        assert!(
            matches!(
                registry.connections.read().await.get("srv"),
                Some(McpConnectionState::Disconnected { .. })
            ),
            "foreground panic must recover to Disconnected instead of stranding Connecting"
        );
        assert!(
            registry.lazy_upgrade_slots.read().await.is_empty(),
            "panic recovery must clear the detached owner slot"
        );
    }

    #[tokio::test]
    async fn foreground_initialize_panic_disconnects_the_known_transport() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        mock.panic_initialize.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        registry.connect(cfg).await.expect("cache hit connect");
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            registry.ensure_dialed_from_cache("srv"),
        )
        .await
        .expect("waiter completes after initialize panic")
        .unwrap_err();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if mock.disconnect_calls.load(Ordering::SeqCst) == 1
                    && mock.conns.lock().unwrap().is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("known transport cleaned up after initialize panic");
        drop(env);

        assert!(
            error.to_string().contains("initialize"),
            "panic after connect must identify the initialize phase"
        );
        assert!(
            matches!(
                registry.connections.read().await.get("srv"),
                Some(McpConnectionState::Disconnected { .. })
            ),
            "foreground initialize panic must recover to Disconnected"
        );
        assert!(registry.lazy_upgrade_slots.read().await.is_empty());
    }

    #[tokio::test]
    async fn background_lazy_upgrade_panic_keeps_cached_state_and_records_a_refresh_failure() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 1_000_000);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        mock.panic_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        let cached_id = registry.connect(cfg).await.expect("stale hit connect");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let entry = match load_test_entry(&store, &cache_key) {
                    crate::discovery_cache::EntryLookup::Found(entry) => entry,
                    other => panic!("expected entry, got {other:?}"),
                };
                if entry.consecutive_refresh_failures == 1
                    && registry.lazy_upgrade_slots.read().await.is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background panic settles");
        drop(env);

        assert!(
            matches!(
                registry.connections.read().await.get("srv"),
                Some(McpConnectionState::Cached { connection_id, .. }) if *connection_id == cached_id
            ),
            "background panic must keep the cached state available"
        );
    }

    #[tokio::test]
    async fn background_post_connect_panic_disconnects_the_known_transport() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 1_000_000);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        mock.panic_list_tools.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        let cached_id = registry.connect(cfg).await.expect("stale cache hit");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let struck = match load_test_entry(&store, &cache_key) {
                    crate::discovery_cache::EntryLookup::Found(entry) => {
                        entry.consecutive_refresh_failures == 1
                    }
                    other => panic!("expected entry, got {other:?}"),
                };
                if struck
                    && registry.lazy_upgrade_slots.read().await.is_empty()
                    && mock.disconnect_calls.load(Ordering::SeqCst) == 1
                    && mock.conns.lock().unwrap().is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("known transport cleaned up after post-connect panic");
        drop(env);

        assert!(
            matches!(
                registry.connections.read().await.get("srv"),
                Some(McpConnectionState::Cached { connection_id, .. }) if *connection_id == cached_id
            ),
            "background post-connect panic must keep the cached generation available"
        );
    }

    #[tokio::test]
    async fn stale_background_failure_does_not_strike_a_replaced_cached_generation() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 1_000_000);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        mock.block_connect.store(true, Ordering::SeqCst);
        mock.list_tools_fails.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        registry.connect(cfg).await.expect("stale hit connect");
        mock.connect_started.notified().await;
        let replacement_id = McpConnectionId::new();
        let lifecycle = registry.lifecycle_lock("srv");
        let _guard = lifecycle.lock().await;
        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Cached {
                config: http_cfg("srv", "https://mcp.example.com/v1"),
                connection_id: replacement_id,
                capabilities: ServerCapabilitiesDto {
                    tools: true,
                    resources: false,
                    prompts: false,
                    directory_read: false,
                    logging: false,
                    experimental: HashMap::new(),
                    extensions: HashMap::new(),
                },
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![McpToolDto {
                    server_name: "srv".into(),
                    tool_name: "replacement".into(),
                    description: "replacement".into(),
                    input_schema: serde_json::json!({"type":"object"}),
                    output_schema: None,
                    annotations: None,
                    icons: Vec::new(),
                    meta: None,
                    full_name: "mcp__srv__replacement".into(),
                    search_hint: None,
                    always_load: None,
                    requires_user_interaction: false,
                }],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                cache_saved_at_ms: 1,
                age_ms: 1,
            },
        );
        seed_entry_with_catalog(
            &store,
            &cache_key,
            1,
            ServerCapabilitiesDto {
                tools: true,
                resources: false,
                prompts: false,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            vec![McpToolDto {
                server_name: "srv".into(),
                tool_name: "replacement".into(),
                description: "replacement".into(),
                input_schema: serde_json::json!({"type":"object"}),
                output_schema: None,
                annotations: None,
                icons: Vec::new(),
                meta: None,
                full_name: "mcp__srv__replacement".into(),
                search_hint: None,
                always_load: None,
                requires_user_interaction: false,
            }],
            vec![],
            vec![],
        );
        drop(_guard);

        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let entry = match load_test_entry(&store, &cache_key) {
                    crate::discovery_cache::EntryLookup::Found(entry) => entry,
                    other => panic!("expected entry, got {other:?}"),
                };
                if entry
                    .tools
                    .iter()
                    .any(|tool| tool.tool_name == "replacement")
                    && entry.consecutive_refresh_failures == 0
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacement entry survives old background failure");
        drop(env);

        assert!(
            matches!(
                registry.connections.read().await.get("srv"),
                Some(McpConnectionState::Cached { connection_id, .. }) if *connection_id == replacement_id
            ),
            "the replacement cached generation must remain current"
        );
    }

    #[tokio::test]
    async fn rejected_background_cleanup_does_not_hold_the_lifecycle_lock() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 1_000_000);

        let mock = Arc::new(BridgeMock::new(&["fresh_live"]));
        mock.block_connect.store(true, Ordering::SeqCst);
        mock.block_disconnect.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        let cached_id = registry.connect(cfg).await.expect("stale hit connect");
        mock.connect_started.notified().await;
        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Cached {
                config: http_cfg("srv", "https://mcp.example.com/v2"),
                connection_id: cached_id,
                capabilities: ServerCapabilitiesDto {
                    tools: true,
                    resources: false,
                    prompts: false,
                    directory_read: false,
                    logging: false,
                    experimental: HashMap::new(),
                    extensions: HashMap::new(),
                },
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                cache_saved_at_ms: 1,
                age_ms: 1_000_000,
            },
        );

        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        mock.disconnect_started.notified().await;

        tokio::time::timeout(Duration::from_millis(200), registry.remove("srv"))
            .await
            .expect("remove must not wait for rejected cleanup disconnect")
            .expect("remove succeeds while cleanup is blocked");
        assert!(
            registry.connections.read().await.get("srv").is_none(),
            "remove should acquire the lifecycle lock even while cleanup waits on transport disconnect"
        );

        mock.block_disconnect.store(false, Ordering::SeqCst);
        mock.disconnect_release.notify_one();
        drop(env);
    }

    #[tokio::test]
    async fn concurrent_stale_cache_hits_share_one_background_revalidation() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 1_000_000);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        mock.block_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        let a = {
            let registry = registry.clone();
            let cfg = cfg.clone();
            tokio::spawn(async move { registry.connect(cfg).await })
        };
        let b = {
            let registry = registry.clone();
            let cfg = cfg.clone();
            tokio::spawn(async move { registry.connect(cfg).await })
        };
        let (a, b) = tokio::join!(a, b);
        let id_a = a.unwrap().expect("first stale hit");
        let id_b = b.unwrap().expect("second stale hit");

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if mock.connect_calls.load(Ordering::SeqCst) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shared background revalidation must start");
        assert_eq!(
            id_a, id_b,
            "both callers must receive the same cached generation"
        );
        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            1,
            "concurrent stale-hit connects must single-flight the background dial"
        );

        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    registry.connections.read().await.get("srv"),
                    Some(McpConnectionState::Connected { .. })
                ) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background revalidation completes");
        drop(env);
    }

    #[tokio::test]
    async fn catalog_refresh_snapshot_recovers_missed_cached_registration() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["live"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        let cached_id = registry.connect(cfg).await.expect("fresh cache hit");
        let mut active = std::collections::HashSet::new();
        apply_lagged_tool_recovery(&registry, &mut active)
            .await
            .expect("snapshot recovery");

        assert_eq!(active, std::collections::HashSet::from([cached_id]));
        drop(env);
    }

    #[tokio::test]
    async fn catalog_refresh_snapshot_excludes_agent_scoped_entries() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["live"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        let shared_id = registry.connect(cfg).await.expect("shared fresh cache hit");
        let scoped_id = McpConnectionId::new();
        registry.connections.write().await.insert(
            "agent:test:srv".into(),
            McpConnectionState::Cached {
                config: http_cfg("srv", "https://mcp.example.com/v1"),
                connection_id: scoped_id,
                capabilities: ServerCapabilitiesDto {
                    tools: true,
                    resources: false,
                    prompts: false,
                    directory_read: false,
                    logging: false,
                    experimental: HashMap::new(),
                    extensions: HashMap::new(),
                },
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![McpToolDto {
                    server_name: "srv".into(),
                    tool_name: "scoped".into(),
                    description: "scoped".into(),
                    input_schema: serde_json::json!({"type":"object"}),
                    output_schema: None,
                    annotations: None,
                    icons: Vec::new(),
                    meta: None,
                    full_name: "mcp__srv__scoped".into(),
                    search_hint: None,
                    always_load: None,
                    requires_user_interaction: false,
                }],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                cache_saved_at_ms: 1,
                age_ms: 0,
            },
        );

        let mut active = std::collections::HashSet::from([scoped_id]);
        apply_lagged_tool_recovery(&registry, &mut active)
            .await
            .expect("snapshot recovery");

        assert_eq!(active, std::collections::HashSet::from([shared_id]));
        drop(env);
    }

    #[tokio::test]
    async fn catalog_refresh_snapshot_recovers_missed_cached_retirement_after_lazy_dial_success() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["live"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        let cached_id = registry.connect(cfg).await.expect("fresh cache hit");
        let live_id = registry
            .ensure_dialed_from_cache("srv")
            .await
            .expect("lazy dial succeeds");
        let (connection, peer_tx, peer_rx) = drivable_connection();
        let live_client = Arc::new(
            McpClient::new("srv", std::path::PathBuf::from("/tmp/work"), connection).await,
        );
        registry.clients.write().await.insert(
            "srv".into(),
            RegisteredClient {
                connection_id: Some(live_id),
                client: live_client,
            },
        );
        let responder = spawn_tools_list_response(peer_tx, peer_rx, "live");
        let mut active = std::collections::HashSet::from([cached_id]);
        apply_lagged_tool_recovery(&registry, &mut active)
            .await
            .expect("snapshot recovery");
        responder.await.expect("tools/list responder");

        assert_eq!(active, std::collections::HashSet::from([live_id]));
        assert!(!active.contains(&cached_id));
        drop(env);
    }

    #[tokio::test]
    async fn catalog_refresh_snapshot_recovers_missed_cached_retirement_after_lazy_dial_partial_failure(
    ) {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::with_drivable_calls(&["live"]));
        mock.list_tools_fails.store(true, Ordering::SeqCst);
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        let cached_id = registry.connect(cfg).await.expect("fresh cache hit");
        let live_id = registry
            .ensure_dialed_from_cache("srv")
            .await
            .expect("lazy dial keeps the transport live on tools/list failure");
        let (peer_tx, peer_rx) = mock
            .take_tool_call_peer(live_id)
            .expect("live generation must expose a drivable client");
        let responder = spawn_tools_list_response(peer_tx, peer_rx, "live");
        let mut active = std::collections::HashSet::from([cached_id]);
        apply_lagged_tool_recovery(&registry, &mut active)
            .await
            .expect("snapshot recovery");
        responder.await.expect("tools/list responder");

        assert_eq!(
            active,
            std::collections::HashSet::from([live_id]),
            "snapshot recovery must retire the cached generation and keep the live one"
        );
        drop(env);
    }

    #[tokio::test]
    async fn background_revalidation_does_not_revive_a_removed_server() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 1_000_000);

        let mock = Arc::new(BridgeMock::new(&["fresh_live"]));
        mock.block_connect.store(true, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        registry
            .connect(cfg)
            .await
            .expect("stale cache hit connect");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if mock.connect_calls.load(Ordering::SeqCst) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background revalidation started");

        let remove = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.remove("srv").await })
        };
        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        remove.await.unwrap().expect("remove after background");
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        drop(env);

        assert!(
            registry.connections.read().await.get("srv").is_none(),
            "queued remove must win over the background refresh"
        );
    }

    #[tokio::test]
    async fn background_revalidation_cas_rejects_a_reconfigured_cached_state() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 1_000_000);

        let mock = Arc::new(BridgeMock::new(&["fresh_live"]));
        mock.block_connect.store(true, Ordering::SeqCst);
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        let cached_id = registry
            .connect(cfg)
            .await
            .expect("stale cache hit connect");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if mock.connect_calls.load(Ordering::SeqCst) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background revalidation started");

        let mut reconfigured = http_cfg("srv", "https://mcp.example.com/v1");
        reconfigured.timeout_ms = Some(1234);
        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Cached {
                config: reconfigured,
                connection_id: cached_id,
                capabilities: ServerCapabilitiesDto {
                    tools: true,
                    resources: false,
                    prompts: false,
                    directory_read: false,
                    logging: false,
                    experimental: HashMap::new(),
                    extensions: HashMap::new(),
                },
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![McpToolDto {
                    server_name: "srv".into(),
                    tool_name: "old".into(),
                    description: "old tool".into(),
                    input_schema: serde_json::json!({"type":"object"}),
                    output_schema: None,
                    annotations: None,
                    icons: Vec::new(),
                    meta: None,
                    full_name: "mcp__srv__old".into(),
                    search_hint: None,
                    always_load: None,
                    requires_user_interaction: false,
                }],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                cache_saved_at_ms: 1,
                age_ms: 1_000_000,
            },
        );

        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if mock.disconnect_calls.load(Ordering::SeqCst) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("discarded background transport is torn down");
        drop(env);

        let conns = registry.connections.read().await;
        let McpConnectionState::Cached { config, tools, .. } =
            conns.get("srv").expect("cached state remains")
        else {
            panic!("reconfigured cached state must remain cached")
        };
        assert_eq!(config.timeout_ms, Some(1234));
        assert_eq!(tools[0].tool_name, "old");
        drop(conns);
        let entry = match load_test_entry(&store, &cache_key) {
            crate::discovery_cache::EntryLookup::Found(entry) => entry,
            other => panic!("seeded cache entry must survive, got {other:?}"),
        };
        assert_eq!(
            entry
                .tools
                .iter()
                .map(|tool| tool.tool_name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha"],
            "CAS failure must not overwrite the persisted cache entry"
        );
    }

    #[tokio::test]
    async fn background_cas_reject_disconnect_retries_until_success() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 1_000_000);

        let mock = Arc::new(BridgeMock::new(&["fresh_live"]));
        mock.block_connect.store(true, Ordering::SeqCst);
        mock.disconnect_failures_remaining
            .store(1, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        let cached_id = registry
            .connect(cfg)
            .await
            .expect("stale cache hit connect");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if mock.connect_calls.load(Ordering::SeqCst) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background revalidation started");

        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Cached {
                config: http_cfg("srv", "https://mcp.example.com/v2"),
                connection_id: cached_id,
                capabilities: ServerCapabilitiesDto {
                    tools: true,
                    resources: false,
                    prompts: false,
                    directory_read: false,
                    logging: false,
                    experimental: HashMap::new(),
                    extensions: HashMap::new(),
                },
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                cache_saved_at_ms: 1,
                age_ms: 1_000_000,
            },
        );

        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        let live_connection_id = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let pending = registry.pending_transport_cleanups.read().await;
                if let Some(connection_id) = pending.keys().copied().next() {
                    break connection_id;
                }
                drop(pending);
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("disconnect failure queued for retry");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if registry
                    .pending_transport_cleanups
                    .read()
                    .await
                    .contains_key(&live_connection_id)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("disconnect failure queued for retry");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if registry.pending_transport_cleanups.read().await.is_empty()
                    && mock.conns.lock().unwrap().is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("retry cleanup succeeds");

        assert_eq!(
            mock.disconnect_calls.load(Ordering::SeqCst),
            2,
            "cleanup must retry after the first disconnect failure"
        );
        assert!(
            registry.pending_transport_cleanups.read().await.is_empty(),
            "successful retry must clear the pending cleanup entry"
        );
        drop(env);
    }

    #[tokio::test]
    async fn background_cleanup_retries_stop_and_can_be_kicked_again() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 1_000_000);

        let mock = Arc::new(BridgeMock::new(&["fresh_live"]));
        mock.block_connect.store(true, Ordering::SeqCst);
        mock.disconnect_failures_remaining
            .store(usize::MAX, Ordering::SeqCst);
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        let cached_id = registry
            .connect(cfg)
            .await
            .expect("stale cache hit connect");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if mock.connect_calls.load(Ordering::SeqCst) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background revalidation started");

        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Cached {
                config: http_cfg("srv", "https://mcp.example.com/v2"),
                connection_id: cached_id,
                capabilities: ServerCapabilitiesDto {
                    tools: true,
                    resources: false,
                    prompts: false,
                    directory_read: false,
                    logging: false,
                    experimental: HashMap::new(),
                    extensions: HashMap::new(),
                },
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                cache_saved_at_ms: 1,
                age_ms: 1_000_000,
            },
        );

        mock.block_connect.store(false, Ordering::SeqCst);
        mock.connect_release.notify_one();
        let live_connection_id = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let pending = registry.pending_transport_cleanups.read().await;
                if let Some((connection_id, entry)) = pending.iter().next() {
                    if !entry.retrying {
                        break *connection_id;
                    }
                }
                drop(pending);
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("bounded retries end and leave a pending entry");

        assert!(
            registry
                .pending_transport_cleanups
                .read()
                .await
                .contains_key(&live_connection_id),
            "permanent disconnect failure must leave an observable pending cleanup"
        );
        assert_eq!(
            mock.disconnect_calls.load(Ordering::SeqCst),
            6,
            "cleanup should stop after one immediate disconnect and five retries"
        );

        mock.disconnect_failures_remaining
            .store(0, Ordering::SeqCst);
        registry.kick_pending_transport_cleanups().await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if registry.pending_transport_cleanups.read().await.is_empty()
                    && mock.conns.lock().unwrap().is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("later kick retries and clears the pending cleanup");
        drop(env);
    }

    #[tokio::test(start_paused = true)]
    async fn hanging_cleanup_disconnect_times_out_and_can_be_kicked_again() {
        let mock = Arc::new(BridgeMock::new(&["read"]));
        let registry = Arc::new(McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        ));
        let connection_id = registry.connect(cfg("mock")).await.unwrap();
        mock.hang_disconnect.store(true, Ordering::SeqCst);

        let cleanup = {
            let registry = registry.clone();
            tokio::spawn(async move {
                registry.disconnect_or_schedule_cleanup(connection_id).await;
            })
        };
        mock.disconnect_started.notified().await;
        tokio::time::advance(cleanup_disconnect_timeout()).await;
        cleanup.await.expect("cleanup task joins");
        tokio::task::yield_now().await;

        drive_cleanup_retry_attempts_for_test().await;
        let mut settled = false;
        for _ in 0..64 {
            let pending = registry.pending_transport_cleanups.read().await;
            if matches!(pending.get(&connection_id), Some(entry) if !entry.retrying) {
                settled = true;
                break;
            }
            drop(pending);
            tokio::task::yield_now().await;
        }
        assert!(
            settled,
            "timed-out cleanup retries must stop and leave a retryable pending entry"
        );

        mock.hang_disconnect.store(false, Ordering::SeqCst);
        registry.kick_pending_transport_cleanups().await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;
        assert!(
            registry.pending_transport_cleanups.read().await.is_empty(),
            "a later kick must retry and clear the timed-out cleanup"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_cleanup_retry_task_does_not_hold_registry_forever() {
        let mock = Arc::new(BridgeMock::new(&["read"]));
        let registry = Arc::new(McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        ));
        let weak = Arc::downgrade(&registry);
        let connection_id = registry.connect(cfg("mock")).await.unwrap();
        mock.hang_disconnect.store(true, Ordering::SeqCst);

        let cleanup = {
            let registry = registry.clone();
            tokio::spawn(async move {
                registry.disconnect_or_schedule_cleanup(connection_id).await;
            })
        };
        mock.disconnect_started.notified().await;
        tokio::time::advance(cleanup_disconnect_timeout()).await;
        cleanup.await.expect("cleanup task joins");
        tokio::task::yield_now().await;
        drop(registry);

        drive_cleanup_retry_attempts_for_test().await;
        let mut released = false;
        for _ in 0..64 {
            if weak.upgrade().is_none() {
                released = true;
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            released,
            "bounded cleanup retries must release the registry once they stop retrying"
        );
    }

    #[tokio::test]
    async fn connect_all_disabled_seed_does_not_override_live_state() {
        let mock = Arc::new(BridgeMock::new(&["read"]));
        let registry = Arc::new(McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        ));
        let lifecycle = registry.lifecycle_lock("srv");
        let guard = lifecycle.lock().await;

        let connect_all = {
            let registry = registry.clone();
            tokio::spawn(async move {
                registry
                    .connect_all(vec![McpServerConfig {
                        disabled: true,
                        ..cfg("srv")
                    }])
                    .await
            })
        };
        tokio::task::yield_now().await;

        let live_id = McpConnectionId::new();
        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Connected {
                config: cfg("srv"),
                connection_id: live_id,
                capabilities: ServerCapabilitiesDto {
                    tools: true,
                    resources: false,
                    prompts: false,
                    directory_read: false,
                    logging: false,
                    experimental: HashMap::new(),
                    extensions: HashMap::new(),
                },
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                connected_at: SystemTime::now(),
            },
        );
        drop(guard);

        assert!(
            connect_all.await.expect("join").is_empty(),
            "disabled connect_all entries remain skipped"
        );
        assert!(
            matches!(
                registry.connections.read().await.get("srv"),
                Some(McpConnectionState::Connected { connection_id, .. }) if *connection_id == live_id
            ),
            "disabled seeding must not overwrite a concurrently-live generation"
        );
    }

    #[tokio::test]
    async fn a_fresh_cache_hit_does_not_start_background_revalidation() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["should_never_be_dialed"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        registry
            .connect(cfg)
            .await
            .expect("fresh cache hit connect");
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        drop(env);

        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            0,
            "a Fresh cache hit must not schedule a background revalidation dial"
        );
    }

    #[tokio::test]
    async fn refresh_catalog_treats_resource_only_tools_change_as_rebuild_signal() {
        let mock = Arc::new(BridgeMock::new(&[]));
        mock.list_tools_fails.store(true, Ordering::SeqCst);
        let registry = McpRegistry::new(mock as Arc<dyn McpTransport>);
        let connection_id = McpConnectionId::new();
        registry.connections.write().await.insert(
            "srv".into(),
            McpConnectionState::Connected {
                config: http_cfg("srv", "https://mcp.example.com/v1"),
                connection_id,
                capabilities: ServerCapabilitiesDto {
                    tools: false,
                    resources: true,
                    prompts: false,
                    directory_read: false,
                    logging: false,
                    experimental: HashMap::new(),
                    extensions: HashMap::new(),
                },
                negotiated: platform_api::McpNegotiatedProtocol {
                    era: platform_api::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                connected_at: SystemTime::now(),
            },
        );

        assert_eq!(
            registry
                .refresh_catalog(&McpCatalogChanged {
                    server_name: "srv".into(),
                    connection_id,
                    retired_connection_id: None,
                    kind: McpCatalogKind::Tools,
                    telemetry_cause: None,
                })
                .await
                .expect("resource-only tools refresh should short-circuit"),
            Some(connection_id),
            "lag recovery must still rebuild the resource-tool partition for a resource-only server"
        );
    }

    #[tokio::test]
    async fn connected_generation_is_not_visible_without_matching_client() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let mock = Arc::new(BridgeMock::with_resource_templates(vec![
            platform_api::McpResourceTemplateDto {
                uri_template: "file:///{path}".into(),
                name: "tmpl".into(),
                description: None,
                mime_type: None,
                annotations: None,
                meta: None,
            },
        ]));
        mock.block_resource_templates.store(true, Ordering::SeqCst);
        let registry = Arc::new(McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        ));
        let clients_guard = registry.clients.read().await;
        let connect = {
            let registry = registry.clone();
            tokio::spawn(async move {
                registry
                    .connect(http_cfg("srv", "https://mcp.example.com/v1"))
                    .await
            })
        };

        mock.resource_templates_started.notified().await;
        mock.block_resource_templates.store(false, Ordering::SeqCst);
        mock.resource_templates_release.notify_one();
        let visible_while_client_locked = tokio::time::timeout(Duration::from_millis(50), async {
            loop {
                match registry.connections.read().await.get("srv") {
                    Some(McpConnectionState::Connected { .. }) => break,
                    _ => tokio::task::yield_now().await,
                }
            }
        })
        .await;
        assert!(
            visible_while_client_locked.is_err(),
            "a Connected generation must not publish before its client can publish"
        );
        drop(clients_guard);

        let connection_id = connect.await.unwrap().expect("connect succeeds");
        assert!(
            matches!(
                registry.connections.read().await.get("srv"),
                Some(McpConnectionState::Connected { connection_id: current, .. }) if *current == connection_id
            ),
            "state publishes once the client lock is released"
        );
        assert!(
            registry.get_client("srv").await.is_some(),
            "a visible Connected generation must have a matching client"
        );
        drop(env);
    }

    #[tokio::test]
    async fn builders_reject_mutation_after_first_connect_attempt() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["live"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        registry
            .connect_all(vec![McpServerConfig {
                disabled: true,
                ..cfg.clone()
            }])
            .await;
        assert_eq!(
            registry.set_disabled("srv", false).await.unwrap(),
            Some(platform_api::McpActionState::Connected)
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registry.with_headers_helper_cwd(std::path::PathBuf::from("/tmp/other"))
        }));
        assert!(
            result.is_err(),
            "builders must reject post-connect mutation"
        );
        drop(env);
    }

    /// The other half of the Stage 2 claim: the FIRST tool call against a
    /// `Cached` server dials the transport EXACTLY ONCE (the lazy dial),
    /// after which the state is a real `Connected` — not still `Cached`,
    /// and not re-dialed a second time by the same call.
    #[tokio::test]
    async fn first_tool_call_against_a_cached_server_dials_exactly_once() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        registry.connect(cfg).await.expect("cache hit connect");
        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            0,
            "precondition: the cache hit must not have dialed yet"
        );

        // Dispatch a tool call. The mock's paired jsonrpc connection has no
        // live peer (see `paired_connection`'s doc), so the RPC itself may
        // fail — this test only asserts that the DIAL happened, not that the
        // round-trip succeeded.
        let _ = registry
            .call_tool_with_auth_retry("srv", "mcp__srv__alpha", serde_json::json!({}), None, None)
            .await;
        drop(env);

        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            1,
            "the first tool call against a Cached server must dial EXACTLY ONCE"
        );
        let conns = registry.connections.read().await;
        assert!(
            matches!(conns.get("srv"), Some(McpConnectionState::Connected { .. })),
            "the lazy dial must upgrade Cached to a real Connected, got {:?}",
            conns.get("srv")
        );
    }

    #[tokio::test]
    async fn ensure_connected_client_rereads_the_live_client_after_publish_race() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let miss_hook = Arc::new(TestPauseHook::default());
        let publish_hook = Arc::new(TestPauseHook::default());
        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(
                dir.path(),
            ))
            .with_pause_after_initial_client_miss(miss_hook.clone())
            .with_pause_before_client_publish(publish_hook.clone()),
        );

        registry.connect(cfg).await.expect("cache hit connect");
        let clients_guard = registry.clients.read().await;
        let caller = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.ensure_connected_client("srv").await })
        };
        miss_hook.entered.notified().await;

        let owner = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
        };
        publish_hook.entered.notified().await;
        assert!(
            !caller.is_finished(),
            "caller must be paused after its initial client miss"
        );

        miss_hook.release.notify_one();
        assert!(
            !caller.is_finished(),
            "caller must still be blocked while publish holds the connections writer"
        );

        publish_hook.release.notify_one();
        drop(clients_guard);

        let live_id = tokio::time::timeout(Duration::from_secs(2), owner)
            .await
            .expect("owner finishes")
            .expect("owner join succeeds")
            .expect("owner publishes L1");
        let client = tokio::time::timeout(Duration::from_secs(2), caller)
            .await
            .expect("caller finishes")
            .expect("caller join succeeds")
            .expect("caller observes the consistency re-read");
        let published = registry
            .get_client("srv")
            .await
            .expect("published live client");
        drop(env);

        assert!(
            Arc::ptr_eq(&client, &published),
            "consistency re-read must return the L1 client that publish inserted"
        );
        assert_eq!(
            registry
                .clients
                .read()
                .await
                .get("srv")
                .and_then(|entry| entry.connection_id),
            Some(live_id)
        );
        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            1,
            "the race recovery must not redial"
        );
    }

    #[tokio::test]
    async fn call_tool_rereads_the_live_client_after_publish_race() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let miss_hook = Arc::new(TestPauseHook::default());
        let publish_hook = Arc::new(TestPauseHook::default());
        let mock = Arc::new(BridgeMock::with_drivable_calls(&["alpha"]));
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(
                dir.path(),
            ))
            .with_pause_after_initial_client_miss(miss_hook.clone())
            .with_pause_before_client_publish(publish_hook.clone()),
        );

        registry.connect(cfg).await.expect("cache hit connect");
        let clients_guard = registry.clients.read().await;
        let input = serde_json::json!({"city": "sf"});
        let caller = {
            let registry = registry.clone();
            let input = input.clone();
            tokio::spawn(async move {
                registry
                    .call_tool_with_auth_retry("srv", "mcp__srv__alpha", input, None, None)
                    .await
            })
        };
        miss_hook.entered.notified().await;

        let owner = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
        };
        publish_hook.entered.notified().await;
        assert!(
            !caller.is_finished(),
            "tool caller must be paused after its initial client miss"
        );
        let live_id = *mock
            .conns
            .lock()
            .unwrap()
            .keys()
            .next()
            .expect("connected raw id before publish completes");
        let (peer_tx, peer_rx) = mock
            .take_tool_call_peer(live_id)
            .expect("tool-call peer for L1");
        let responder = spawn_tool_call_response(peer_tx, peer_rx, "alpha", input.clone());

        miss_hook.release.notify_one();
        assert!(
            !caller.is_finished(),
            "tool caller must still be blocked behind publish before the consistency re-read"
        );

        publish_hook.release.notify_one();
        drop(clients_guard);

        let owner_id = tokio::time::timeout(Duration::from_secs(2), owner)
            .await
            .expect("owner finishes")
            .expect("owner join succeeds")
            .expect("owner publishes L1");
        let result = tokio::time::timeout(Duration::from_secs(2), caller)
            .await
            .expect("tool caller finishes")
            .expect("tool caller join succeeds")
            .expect("tool call succeeds through the published client");
        responder.await.expect("tool responder");
        drop(env);

        assert_eq!(owner_id, live_id);
        assert_eq!(
            result.structured_content,
            Some(serde_json::json!({"tool":"alpha","input":{"city":"sf"}}))
        );
        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            1,
            "the publish-race recovery must reuse L1 instead of redialing"
        );
    }

    #[tokio::test]
    async fn call_tool_publish_race_client_still_retries_a_first_auth_challenge() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let miss_hook = Arc::new(TestPauseHook::default());
        let publish_hook = Arc::new(TestPauseHook::default());
        let mock = Arc::new(BridgeMock::with_drivable_calls(&["alpha"]));
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(
                dir.path(),
            ))
            .with_pause_after_initial_client_miss(miss_hook.clone())
            .with_pause_before_client_publish(publish_hook.clone()),
        );

        registry.connect(cfg).await.expect("cache hit connect");
        let clients_guard = registry.clients.read().await;
        let input = serde_json::json!({"city": "sf"});
        let caller = {
            let registry = registry.clone();
            let input = input.clone();
            tokio::spawn(async move {
                registry
                    .call_tool_with_auth_retry("srv", "mcp__srv__alpha", input, None, None)
                    .await
            })
        };
        miss_hook.entered.notified().await;

        let owner = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.ensure_dialed_from_cache("srv").await })
        };
        publish_hook.entered.notified().await;
        let first_live_id = *mock
            .conns
            .lock()
            .unwrap()
            .keys()
            .next()
            .expect("connected raw id before publish completes");
        let responder = spawn_tool_call_auth_then_success(
            mock.clone(),
            Some(first_live_id),
            "alpha",
            input,
            401,
        );

        miss_hook.release.notify_one();
        publish_hook.release.notify_one();
        publish_hook.release.notify_one();
        drop(clients_guard);

        let owner_id = tokio::time::timeout(Duration::from_secs(2), owner)
            .await
            .expect("owner finishes")
            .expect("owner join succeeds")
            .expect("owner publishes L1");
        let result = tokio::time::timeout(Duration::from_secs(2), caller)
            .await
            .expect("tool caller finishes")
            .expect("tool caller join succeeds")
            .expect("tool call succeeds after reconnect retry");
        responder.await.expect("auth retry responder");
        let refreshed_id = registry
            .clients
            .read()
            .await
            .get("srv")
            .and_then(|entry| entry.connection_id)
            .expect("refreshed client id");
        drop(env);

        assert_eq!(owner_id, first_live_id);
        assert_ne!(
            refreshed_id, first_live_id,
            "a first auth challenge must drive the reconnect path to a fresh generation"
        );
        assert_eq!(
            result.structured_content,
            Some(serde_json::json!({"tool":"alpha","input":{"city":"sf"}}))
        );
        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            2,
            "publish-race client recovery must still flow into the single reconnect retry"
        );
    }

    #[tokio::test]
    async fn cached_lazy_dial_client_still_retries_a_first_auth_challenge() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::with_drivable_calls(&["alpha"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        registry.connect(cfg).await.expect("cache hit connect");
        let input = serde_json::json!({"city": "sf"});
        let responder =
            spawn_tool_call_auth_then_success(mock.clone(), None, "alpha", input.clone(), 401);
        let result = registry
            .call_tool_with_auth_retry("srv", "mcp__srv__alpha", input, None, None)
            .await
            .expect("tool call succeeds after cached lazy-dial auth retry");
        responder.await.expect("auth retry responder");
        let refreshed_id = registry
            .clients
            .read()
            .await
            .get("srv")
            .and_then(|entry| entry.connection_id)
            .expect("refreshed client id");
        drop(env);

        assert_eq!(
            result.structured_content,
            Some(serde_json::json!({"tool":"alpha","input":{"city":"sf"}}))
        );
        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            2,
            "cached lazy dial plus first auth challenge must use exactly one reconnect retry"
        );
        assert!(
            mock.conns.lock().unwrap().contains_key(&refreshed_id),
            "the post-retry client must point at the refreshed generation"
        );
    }

    #[test]
    fn session_expired_base_url_dimension_is_normalized_and_hashed() {
        let secret = http_cfg(
            "srv",
            "https://alice:password@mcp.example.com/v1/?token=secret#fragment",
        );
        let clean = http_cfg("srv", "https://mcp.example.com/v1");
        let doubled = http_cfg("srv", "https://mcp.example.com/v1//");
        let secret_hash = telemetry_mcp_server_base_url(&secret.spec).unwrap();
        let clean_hash = telemetry_mcp_server_base_url(&clean.spec).unwrap();
        let doubled_hash = telemetry_mcp_server_base_url(&doubled.spec).unwrap();
        assert_eq!(secret_hash.as_str(), clean_hash.as_str());
        assert_ne!(
            doubled_hash.as_str(),
            clean_hash.as_str(),
            "oracle removes exactly one trailing slash"
        );
        assert_eq!(secret_hash.as_str().len(), 12);
        assert!(secret_hash
            .as_str()
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn http_tool_call_session_expired_reconnects_once_and_retries() {
        let _capture = test_telemetry_capture_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_test_telemetry_events();
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let mock = Arc::new(BridgeMock::with_drivable_calls(&["alpha"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        );
        registry.connect(cfg).await.expect("connect");

        let input = serde_json::json!({"city": "sf"});
        let responder = spawn_tool_call_session_expired_then_success(
            mock.clone(),
            None,
            "alpha",
            input.clone(),
        );
        let result = registry
            .call_tool_with_auth_retry("srv", "mcp__srv__alpha", input, None, None)
            .await
            .expect("tool call succeeds after session-expired reconnect");
        responder.await.expect("session expired responder");

        let refreshed_id = registry
            .clients
            .read()
            .await
            .get("srv")
            .and_then(|entry| entry.connection_id)
            .expect("refreshed client id");
        assert_eq!(
            result.structured_content,
            Some(serde_json::json!({"tool":"alpha","input":{"city":"sf"}}))
        );
        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            2,
            "a session-expired tool call must use exactly one reconnect retry"
        );
        assert!(
            mock.conns.lock().unwrap().contains_key(&refreshed_id),
            "the post-retry client must point at the refreshed generation"
        );
        let events = take_test_telemetry_events();
        let event = events
            .iter()
            .find(|event| event.name == telemetry::tengu::mcp::SESSION_EXPIRED)
            .expect("session expired telemetry");
        assert_eq!(event.payload["errorCode"], serde_json::json!("404"));
        assert_eq!(event.payload["transportType"], serde_json::json!("http"));
        assert!(event.payload["mcpServerKeyHash"].as_str().is_some());
        let base_url_hash = event.payload["mcpServerBaseUrl"]
            .as_str()
            .expect("base URL hash");
        assert_eq!(base_url_hash.len(), 12);
        assert!(base_url_hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(!base_url_hash.contains("mcp.example.com"));
        assert!(event.payload.get("mcpServerName").is_none());
        assert!(event.payload.get("mcpToolName").is_none());
    }

    #[tokio::test]
    async fn stale_session_reconnect_preserves_oauth_grant() {
        struct UnusedHttp;
        #[async_trait]
        impl platform_api::HttpTransport for UnusedHttp {
            async fn request(
                &self,
                _req: protocol::HttpRequest,
            ) -> Result<protocol::HttpResponse, platform_api::HttpError> {
                Err(platform_api::HttpError::InvalidRequest("unused".into()))
            }

            async fn stream_sse(
                &self,
                _req: protocol::HttpRequest,
            ) -> Result<platform_api::http::SseStream, platform_api::HttpError> {
                Err(platform_api::HttpError::InvalidRequest("unused".into()))
            }
        }

        let mut cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let McpTransportSpec::Http { oauth, .. } = &mut cfg.spec else {
            unreachable!()
        };
        *oauth = Some(platform_api::McpOAuthConfigDto {
            client_id: Some("client-id".into()),
            callback_port: None,
            auth_server_metadata_url: None,
            scopes: None,
            xaa: None,
        });
        let clock = Arc::new(FixedClock(
            std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
        ));
        let storage = Arc::new(XaaMemStorage::default());
        let storage_dyn = storage.clone() as Arc<dyn platform_api::SecureStorage>;
        let key = oauth::server_key(&cfg.name, &cfg.spec);
        oauth::store_tokens(
            &storage_dyn,
            &(clock.clone() as Arc<dyn platform_api::Clock>),
            &key,
            &oauth::StoredTokens {
                access_token: "still-valid".into(),
                refresh_token: Some("refresh".into()),
                expires_at_unix: 10_000,
                client_id: Some("client-id".into()),
                client_secret: None,
                step_up_scope: None,
            },
        )
        .await
        .expect("store grant");

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        )
        .with_oauth(OAuthDeps {
            http: Arc::new(UnusedHttp),
            clock: clock as Arc<dyn platform_api::Clock>,
            storage: storage_dyn.clone(),
            on_authorization_url: Arc::new(|_| {}),
            xaa_config: None,
        });
        registry.connect(cfg).await.expect("connect");
        registry
            .reconnect_preserving_auth("srv")
            .await
            .expect("stale-session reconnect");
        assert!(oauth::load_tokens(&storage_dyn, &key)
            .await
            .expect("load grant")
            .is_some());
    }

    #[tokio::test]
    async fn connect_oauth_discovery_failure_emits_server_needs_auth_with_discovery_schema_cause() {
        let _capture = test_telemetry_capture_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        struct DiscoveryFailHttp;

        #[async_trait]
        impl platform_api::HttpTransport for DiscoveryFailHttp {
            async fn request(
                &self,
                req: protocol::HttpRequest,
            ) -> Result<protocol::HttpResponse, platform_api::HttpError> {
                let status = if req.url.contains("oauth-protected-resource")
                    || req.url.contains("oauth-authorization-server")
                {
                    404
                } else {
                    500
                };
                Ok(protocol::HttpResponse {
                    status,
                    headers: vec![],
                    body: String::new(),
                    body_bytes: Vec::new(),
                })
            }

            async fn stream_sse(
                &self,
                _req: protocol::HttpRequest,
            ) -> Result<platform_api::http::SseStream, platform_api::HttpError> {
                Err(platform_api::HttpError::InvalidRequest("unused".into()))
            }
        }

        clear_test_telemetry_events();

        let mut cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let McpTransportSpec::Http { oauth, .. } = &mut cfg.spec else {
            unreachable!()
        };
        *oauth = Some(platform_api::McpOAuthConfigDto {
            client_id: Some("client-id".into()),
            callback_port: None,
            auth_server_metadata_url: None,
            scopes: None,
            xaa: None,
        });

        let storage = Arc::new(XaaMemStorage::default());
        let storage_dyn = storage.clone() as Arc<dyn platform_api::SecureStorage>;
        let clock = Arc::new(FixedClock(std::time::UNIX_EPOCH)) as Arc<dyn platform_api::Clock>;
        let server_key = oauth::server_key(&cfg.name, &cfg.spec);
        oauth::store_tokens(
            &storage_dyn,
            &clock,
            &server_key,
            &oauth::StoredTokens {
                access_token: "expired".into(),
                refresh_token: Some("refresh".into()),
                expires_at_unix: 0,
                client_id: Some("client-id".into()),
                client_secret: None,
                step_up_scope: None,
            },
        )
        .await
        .expect("store expired tokens");

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        )
        .with_oauth(OAuthDeps {
            http: Arc::new(DiscoveryFailHttp),
            clock,
            storage: storage_dyn,
            on_authorization_url: Arc::new(|_| {}),
            xaa_config: None,
        });

        let error = registry.connect(cfg).await.expect_err("connect must fail");
        assert!(
            matches!(error, McpError::OAuth(_) | McpError::Connection(_)),
            "expected oauth/path failure, got {error:?}"
        );

        let events = take_test_telemetry_events();
        let event = events
            .iter()
            .find(|event| event.name == telemetry::tengu::mcp::SERVER_NEEDS_AUTH)
            .expect("server needs auth telemetry");
        assert_eq!(
            event.payload["cause"],
            serde_json::json!("discovery_schema")
        );
        assert_eq!(event.payload["transport_type"], serde_json::json!("http"));
        assert!(event.payload["mcp_server_key_hash"].as_str().is_some());
    }

    #[tokio::test]
    async fn second_auth_failure_emits_tool_call_auth_error() {
        let _capture = test_telemetry_capture_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_test_telemetry_events();

        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let mock = Arc::new(BridgeMock::with_drivable_calls(&["alpha"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        );
        registry.connect(cfg).await.expect("connect");

        let input = serde_json::json!({"city": "sf"});
        let responder =
            spawn_tool_call_auth_then_auth(mock.clone(), None, "alpha", input.clone(), 403);
        let error = registry
            .call_tool_with_auth_retry("srv", "mcp__srv__alpha", input, None, None)
            .await
            .expect_err("second auth failure must surface");
        responder.await.expect("double-auth responder");

        assert!(
            error.is_auth_response(),
            "expected auth-shaped error, got {error:?}"
        );
        let events = take_test_telemetry_events();
        let event = events
            .iter()
            .find(|event| event.name == telemetry::tengu::mcp::TOOL_CALL_AUTH_ERROR)
            .expect("tool call auth error telemetry");
        assert_eq!(event.payload["error_code"], serde_json::json!("403"));
        assert_eq!(
            event.payload["auth_error_kind"],
            serde_json::json!("token_expired")
        );
        assert_eq!(event.payload["transport_type"], serde_json::json!("http"));
        assert!(event.payload["mcp_server_key_hash"].as_str().is_some());
    }

    /// Single-flight: two CONCURRENT tool calls against the same `Cached`
    /// server must dial the transport exactly ONCE between them — the
    /// second caller blocks on the same per-server lifecycle lock `connect`
    /// already uses, then observes `Connected` and never dials again.
    #[tokio::test]
    async fn concurrent_tool_calls_against_a_cached_server_dial_only_once() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        let registry = Arc::new(
            McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock.clone() as Arc<dyn RawConnectionProvider>,
            )
            .with_discovery_cache_store(
                crate::discovery_cache::DiscoveryCacheStore::new(dir.path()),
            ),
        );

        registry.connect(cfg).await.expect("cache hit connect");
        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            0,
            "precondition: the cache hit must not have dialed yet — otherwise \
             this test cannot distinguish single-flighting the lazy dial from \
             simply never having anything left to single-flight"
        );

        let a = {
            let registry = registry.clone();
            tokio::spawn(async move {
                let _ = registry
                    .call_tool_with_auth_retry(
                        "srv",
                        "mcp__srv__alpha",
                        serde_json::json!({}),
                        None,
                        None,
                    )
                    .await;
            })
        };
        let b = {
            let registry = registry.clone();
            tokio::spawn(async move {
                let _ = registry
                    .call_tool_with_auth_retry(
                        "srv",
                        "mcp__srv__alpha",
                        serde_json::json!({}),
                        None,
                        None,
                    )
                    .await;
            })
        };
        let _ = tokio::join!(a, b);
        drop(env);

        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            1,
            "two concurrent tool calls against one Cached server must dial exactly ONCE"
        );
    }

    /// `/mcp disconnect` on a `Cached` server must transition it to
    /// `Stopped` WITHOUT calling `McpTransport::disconnect` — there is no
    /// live transport connection behind a cache-served entry to tear down
    /// (its `connection_id` is a synthetic one; see
    /// `McpConnectionState::Cached`'s doc).
    #[tokio::test]
    async fn disconnecting_a_cached_server_never_touches_the_transport() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["alpha"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        registry.connect(cfg).await.expect("cache hit connect");
        registry.disconnect("srv").await.expect("disconnect");
        drop(env);

        assert_eq!(
            mock.disconnect_calls.load(Ordering::SeqCst),
            0,
            "a Cached server's teardown must never call transport disconnect"
        );
        let conns = registry.connections.read().await;
        assert!(
            matches!(conns.get("srv"), Some(McpConnectionState::Stopped { .. })),
            "expected Stopped state, got {:?}",
            conns.get("srv")
        );
        drop(conns);
        assert!(
            !registry.has_callable_server("srv").await,
            "a stopped server must not report callable"
        );
    }

    /// A `Cached` server must contribute to [`McpRegistry::servers_with_tools`]
    /// exactly like a `Connected` one — `AgentTool`'s required-MCP gate must
    /// not refuse a subagent spawn naming a server the model's own tool list
    /// already shows as available (`build_registered_mcp_tools` gets the same
    /// treatment, in `tool-mcp`).
    #[tokio::test]
    async fn servers_with_tools_includes_a_cached_server() {
        let _guard = crate::discovery_cache::tests_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let env = DiscoveryCacheEnvGuard::new();
        env.set(crate::discovery_cache::ENV_ENABLED, "true");

        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::discovery_cache::DiscoveryCacheStore::new(dir.path());
        let cfg = http_cfg("srv", "https://mcp.example.com/v1");
        let cache_key = crate::discovery_cache::logical_cache_key(&cfg);
        seed_entry(&store, &cache_key, 0);

        let mock = Arc::new(BridgeMock::new(&["should_never_be_dialed"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock.clone() as Arc<dyn RawConnectionProvider>,
        )
        .with_discovery_cache_store(crate::discovery_cache::DiscoveryCacheStore::new(dir.path()));

        registry.connect(cfg).await.expect("cache hit connect");
        drop(env);

        assert_eq!(
            registry.servers_with_tools().await,
            vec!["srv".to_string()],
            "a Cached server's tools must count toward servers_with_tools"
        );
        assert_eq!(
            mock.connect_calls.load(Ordering::SeqCst),
            0,
            "servers_with_tools must not have triggered a dial either"
        );
    }

    /// Pure-function coverage of `discovery_source_emission`, the helper
    /// `connect_locked_inner`'s MISS branch consults: a `Miss` emits iff
    /// `crate::discovery_cache::miss_emits_discovery_source_telemetry` says
    /// so, with the exact `miss_telemetry_value` string. A `Fresh`/`Stale`
    /// decision is asserted `None` here too, but that is this PURE HELPER's
    /// contract, not the whole connect path any more (§11 Stage 2): a real
    /// cache hit is served by `serve_discovery_cache_hit`, a SEPARATE code
    /// path that emits its own `"cache_fresh"`/`"cache_stale"` telemetry with
    /// the real `entryAgeMs` — see
    /// `a_fresh_cache_hit_serves_without_dialing_and_emits_cache_fresh`.
    #[test]
    fn discovery_source_emission_matches_the_miss_gate() {
        use crate::discovery_cache::{Decision, DiscoveryCacheEntry, MissReason};

        assert_eq!(
            discovery_source_emission(&Decision::Miss {
                reason: MissReason::Absent
            }),
            Some("live")
        );
        assert_eq!(
            discovery_source_emission(&Decision::Miss {
                reason: MissReason::Expired
            }),
            Some("miss_expired")
        );
        assert_eq!(
            discovery_source_emission(&Decision::Miss {
                reason: MissReason::Disabled
            }),
            None,
            "a gate-level miss must not emit"
        );
        assert_eq!(
            discovery_source_emission(&Decision::Miss {
                reason: MissReason::Transport
            }),
            None,
            "a gate-level miss must not emit"
        );
        let entry = DiscoveryCacheEntry::new(
            "k".into(),
            1,
            ServerCapabilitiesDto::default(),
            vec![],
            vec![],
            vec![],
            vec![],
        );
        assert_eq!(
            discovery_source_emission(&Decision::Fresh {
                entry: entry.clone(),
                age_ms: 1
            }),
            None,
            "this pure helper never handles a HIT — `serve_discovery_cache_hit` does"
        );
        assert_eq!(
            discovery_source_emission(&Decision::Stale { entry, age_ms: 1 }),
            None,
            "this pure helper never handles a HIT — `serve_discovery_cache_hit` does"
        );
    }

    /// §20a runs on the CONNECT path, not just on `McpClient::list_tools`.
    ///
    /// `McpRegistry::connect` fills `McpConnectionState::Connected { tools }`
    /// from `self.transport.list_tools(...)` — the posix transport, which
    /// never touches `tool_schema`. That is the list
    /// `build_registered_mcp_tools` hands the model on every desktop session;
    /// `McpClient::list_tools` (where the decision already lived) is reached
    /// only by `refresh_catalog`. Without the decision here a root-`anyOf`
    /// schema the oracle drops is forwarded to the model verbatim, and the
    /// two `mock_mcp.rs` integration tests that cover `McpClient::list_tools`
    /// stay green throughout.
    // Same rationale as `connect_resolves_the_schema_gate_from_the_servers_own_hostname`
    // below: the §20a flags are PROCESS-global, so the guard must span the
    // `connect` await — holding it across the await IS the point of the lock.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn connect_applies_the_tool_schema_decision_to_the_model_facing_list() {
        // This test asserts the DEFAULT (both gates off) behaviour by reading
        // the PROCESS-GLOBAL §20a flags, so it must hold the same lock every
        // other §20a test in this binary holds — see
        // `tool_schema::flag_test_lock`. Without it a concurrently-running
        // `tool_schema` test that sets `tengu_mcp_normalize_root_combinators`
        // makes `combo_tool` survive here and this assertion fails at random.
        let _g = crate::tool_schema::flag_test_lock();
        let mock = Arc::new(BridgeMock::with_tool_schemas(&[
            (
                "plain_tool",
                serde_json::json!({"type": "object", "properties": {"a": {"type": "string"}}}),
            ),
            (
                "combo_tool",
                serde_json::json!({"anyOf": [
                    {"type": "object", "properties": {"a": {"type": "string"}}},
                    {"type": "object", "properties": {"b": {"type": "string"}}}
                ]}),
            ),
        ]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        );
        registry.connect(cfg("combos")).await.unwrap();

        let conns = registry.connections.read().await;
        let McpConnectionState::Connected { tools, .. } = conns.get("combos").unwrap() else {
            panic!("expected Connected state");
        };
        let names: Vec<&str> = tools.iter().map(|t| t.tool_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["plain_tool"],
            "the root-anyOf tool must be dropped from the CONNECTED tool list \
             (the normalize gate is off by default), leaving the plain tool: {tools:?}"
        );
        assert_eq!(tools[0].full_name, "mcp__combos__plain_tool");
    }

    /// §20b — connecting a server with two droppable tools must still leave
    /// only the healthy tool in the model-facing list (the `retain_mut`
    /// aggregation change must not perturb the KEEP/DROP decision itself).
    /// The aggregated `tengu_mcp_degraded` payload-building itself is unit
    /// tested directly on `degraded_payloads_for_server` below — NOT via a
    /// tracing capture here, deliberately: `tracing::subscriber::set_default`
    /// is thread-local, but callsite `Interest` caching is process-global, so
    /// a concurrently-running test's subscriber can race the cache and
    /// silently starve this one's captured events under `cargo test`'s
    /// default parallelism (confirmed empirically: green alone under
    /// `--test-threads=1`, flaky in the full suite).
    #[tokio::test]
    async fn connect_still_drops_both_anyof_tools_with_aggregation_wired() {
        let mock = Arc::new(BridgeMock::with_tool_schemas(&[
            (
                "plain_tool",
                serde_json::json!({"type": "object", "properties": {"a": {"type": "string"}}}),
            ),
            (
                "combo_one",
                serde_json::json!({"anyOf": [
                    {"type": "object", "properties": {"a": {"type": "string"}}}
                ]}),
            ),
            (
                "combo_two",
                serde_json::json!({"anyOf": [
                    {"type": "object", "properties": {"b": {"type": "string"}}}
                ]}),
            ),
        ]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        );
        registry.connect(cfg("degraded_combos")).await.unwrap();

        let conns = registry.connections.read().await;
        let McpConnectionState::Connected { tools, .. } = conns.get("degraded_combos").unwrap()
        else {
            panic!("expected Connected state");
        };
        let names: Vec<&str> = tools.iter().map(|t| t.tool_name.as_str()).collect();
        assert_eq!(names, vec!["plain_tool"]);
    }

    /// §20b — `degraded_payloads_for_server` (the pure aggregation step) maps
    /// each nonzero classification bucket to exactly one payload, with the
    /// right count field populated. Reverting the match arms (e.g. routing
    /// `ToolSchemaUnsupported` to `normalized_count`, or emitting one payload
    /// per tool instead of per bucket) is caught here with no tracing
    /// dependency at all.
    #[test]
    fn degraded_payloads_for_server_maps_each_bucket_to_its_own_count_field() {
        use std::collections::HashMap;
        use telemetry::tengu::mcp::DegradedReason;
        use telemetry::Verified;

        let mut counts = HashMap::new();
        counts.insert(DegradedReason::ToolSchemaNormalized, 3);
        counts.insert(DegradedReason::ToolSchemaNormalizeGated, 2);
        counts.insert(DegradedReason::ToolSchemaUnsupported, 1);
        counts.insert(DegradedReason::ToolSchemaInvalid, 4);
        counts.insert(DegradedReason::ToolPropertyKeyInvalid, 5);
        counts.insert(DegradedReason::ToolSchemaInvalidGated, 6);
        counts.insert(DegradedReason::ToolPropertyKeyInvalidGated, 7);

        let mut payloads = degraded_payloads_for_server(&counts, "http", "srv");
        payloads.sort_by_key(|p| p.reason.wire_str());

        let by_reason: std::collections::HashMap<&'static str, _> = payloads
            .iter()
            .map(|p| {
                (
                    p.reason.wire_str(),
                    (p.normalized_count, p.skipped_count, p.kept_count),
                )
            })
            .collect();
        assert_eq!(
            payloads.len(),
            7,
            "one payload per nonzero bucket: {payloads:?}"
        );
        assert_eq!(by_reason["tool_schema_normalized"], (Some(3), None, None));
        assert_eq!(
            by_reason["tool_schema_normalize_gated"],
            (None, Some(2), None)
        );
        assert_eq!(by_reason["tool_schema_unsupported"], (None, Some(1), None));
        assert_eq!(by_reason["tool_schema_invalid"], (None, Some(4), None));
        assert_eq!(
            by_reason["tool_property_key_invalid"],
            (None, Some(5), None)
        );
        assert_eq!(
            by_reason["tool_schema_invalid_gated"],
            (None, None, Some(6))
        );
        assert_eq!(
            by_reason["tool_property_key_invalid_gated"],
            (None, None, Some(7))
        );
        for p in &payloads {
            assert_eq!(
                p.transport_type.as_ref().map(Verified::as_str),
                Some("http")
            );
            assert!(
                p.mcp_server_name.is_none(),
                "`http` is user-configurable; the oracle's HT gate drops the name"
            );
        }
    }

    /// ROUND-1 REGRESSION. `connected_zero_tools` is the FIRST statement of
    /// the oracle's `yn` — 20 lines above the seven tool-schema counters the
    /// module doc transcribed verbatim while calling that set complete. The
    /// port emitted nothing for it, so the most common silent-MCP-failure
    /// signal (an OAuth-pending, resources-only, or fully-filtered server)
    /// was invisible.
    ///
    /// NOTE ON COVERAGE: this pins the predicate, not the call site. The
    /// aggregated event itself cannot be asserted from a `connect` test here
    /// — see `connect_still_drops_both_anyof_tools_with_aggregation_wired`
    /// for why this file deliberately does no tracing capture. What the call
    /// site must preserve, and what review must check, is that the argument
    /// is the RAW `tools.len()` read BEFORE `retain_mut` filters the list.
    #[test]
    fn connected_zero_tools_fires_only_on_an_empty_raw_list_with_the_tools_capability() {
        assert!(
            connected_zero_tools_fires(true, 0),
            "server advertised tools/list and returned an empty array"
        );
        assert!(
            !connected_zero_tools_fires(true, 2),
            "a NON-empty raw list never fires it, however many tools the \u{a7}20a filter \
             later drops \u{2014} those report their own drop reason instead"
        );
        assert!(
            !connected_zero_tools_fires(false, 0),
            "with no tools capability the oracle never reaches `yn`, so an empty list is \
             not a degraded signal"
        );
        assert!(!connected_zero_tools_fires(false, 3));
    }

    /// The reason maps to a payload with NO count field — the oracle emits
    /// `{reason,transportType,mcpServerName,..._}` for it.
    #[test]
    fn connected_zero_tools_bucket_becomes_a_countless_payload() {
        let counts = std::collections::HashMap::from([(
            telemetry::tengu::mcp::DegradedReason::ConnectedZeroTools,
            1,
        )]);
        let payloads = degraded_payloads_for_server(&counts, "stdio", "srv");
        assert_eq!(payloads.len(), 1, "one payload for the one nonzero bucket");
        let p = &payloads[0];
        assert_eq!(p.reason.wire_str(), "connected_zero_tools");
        assert!(p.normalized_count.is_none());
        assert!(p.skipped_count.is_none());
        assert!(p.kept_count.is_none());
        assert_eq!(
            p.transport_type
                .as_ref()
                .map(telemetry::pii::Verified::as_str),
            Some("stdio")
        );
    }

    #[test]
    fn degraded_payloads_for_server_is_empty_when_no_bucket_is_nonzero() {
        assert!(
            degraded_payloads_for_server(&std::collections::HashMap::new(), "stdio", "srv")
                .is_empty()
        );
    }

    /// §20b — `server_config_invalid_payload` carries the RAW config `type`
    /// string (`McpTransportSpec::kind()`, not a `protocol_negotiation.rs`
    /// `Wr`-mapped label), the fixed literal `"url"` field, and passes the
    /// caller's loader-vs-connect classification straight through. Both
    /// `connect()` gates (`config.config_error` / `connect_time_url_error()`)
    /// funnel through this one function, so a test here covers both call
    /// sites' payload shape without needing to race a tracing capture
    /// against `connect()`'s own dial path.
    #[test]
    fn server_config_invalid_payload_carries_the_raw_transport_kind_and_fixed_field() {
        use telemetry::tengu::mcp::ConfigInvalidSource;

        let cfg = http_cfg("broken", "${MISSING:-}");

        let loader = server_config_invalid_payload(&cfg, ConfigInvalidSource::Loader);
        assert_eq!(loader.transport_type.as_str(), "http");
        assert_eq!(loader.field.as_str(), "url");
        assert_eq!(loader.source.wire_str(), "loader");

        let connect = server_config_invalid_payload(&cfg, ConfigInvalidSource::Connect);
        assert_eq!(connect.source.wire_str(), "connect");
    }

    /// §20b — `tools_listed_payload` counts off the FINAL (post-§20a-filter)
    /// list, not a raw pre-filter count, and `always_load_count` only tallies
    /// `Some(true)` (a `None`/`Some(false)` tool must NOT count). Reverting
    /// either the length source or the filter predicate is caught here.
    #[test]
    fn tools_listed_payload_counts_off_the_final_list() {
        let tools = vec![
            McpToolDto {
                tool_name: "a".into(),
                full_name: "mcp__srv__a".into(),
                server_name: "srv".into(),
                description: String::new(),
                input_schema: serde_json::json!({}),
                output_schema: None,
                annotations: None,
                icons: Vec::new(),
                meta: None,
                search_hint: None,
                always_load: Some(true),
                requires_user_interaction: false,
            },
            McpToolDto {
                tool_name: "b".into(),
                full_name: "mcp__srv__b".into(),
                server_name: "srv".into(),
                description: String::new(),
                input_schema: serde_json::json!({}),
                output_schema: None,
                annotations: None,
                icons: Vec::new(),
                meta: None,
                search_hint: None,
                always_load: Some(false),
                requires_user_interaction: false,
            },
            McpToolDto {
                tool_name: "c".into(),
                full_name: "mcp__srv__c".into(),
                server_name: "srv".into(),
                description: String::new(),
                input_schema: serde_json::json!({}),
                output_schema: None,
                annotations: None,
                icons: Vec::new(),
                meta: None,
                search_hint: None,
                always_load: None,
                requires_user_interaction: false,
            },
        ];
        let payload =
            tools_listed_payload("http", std::time::Duration::from_millis(42), &tools, "srv");
        assert_eq!(payload.transport_type.as_str(), "http");
        assert_eq!(payload.list_duration_ms, 42);
        assert_eq!(payload.tool_count, 3);
        assert_eq!(
            payload.always_load_count, 1,
            "only the Some(true) tool counts"
        );
        assert_eq!(payload.discovery_source.as_str(), "live");
        // Gated: `http` is a user-configurable transport, so the oracle's
        // `HT` gate is false and `EA` drops the key entirely.
        assert!(
            payload.mcp_server_name.is_none(),
            "a user-configured server's raw name must never reach telemetry"
        );
    }

    /// §20a's per-server gate resolves from the connected server's URL
    /// hostname (oracle `Ot(e,t)`: `new URL(t.url).hostname`). `connect` must
    /// therefore hand the gate the URL off `config.spec` — otherwise the gate
    /// sees `None` for every server and only a bare `"*"` entry could ever
    /// enable either transform.
    ///
    /// Written under the ORACLE's literal flag key rather than the module's
    /// private constant, so a misspelling there cannot make this green.
    // The §20a flags are PROCESS-global, so the guard must span both `connect`
    // calls — that is the whole point of the lock, and the `mcp` lib test
    // binary runs `tool_schema`'s gate tests in the same process.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn connect_resolves_the_schema_gate_from_the_servers_own_hostname() {
        let _g = crate::tool_schema::flag_test_lock();
        let flag = crate::tool_schema::ORACLE_NORMALIZE_FLAG_FOR_TEST;
        telemetry::test_set_flag_list(flag, vec!["mcp.example.com".to_string()]);

        let listed = |cfg: McpServerConfig| async move {
            let mock = Arc::new(BridgeMock::with_tool_schemas(&[(
                "combo_tool",
                serde_json::json!({"anyOf": [
                    {"type": "object", "properties": {"a": {"type": "string"}}}
                ]}),
            )]));
            let registry = McpRegistry::with_raw_conn(
                mock.clone() as Arc<dyn McpTransport>,
                mock as Arc<dyn RawConnectionProvider>,
            );
            let name = cfg.name.clone();
            registry.connect(cfg).await.unwrap();
            let conns = registry.connections.read().await;
            let McpConnectionState::Connected { tools, .. } = conns.get(&name).unwrap() else {
                panic!("expected Connected state");
            };
            tools.clone()
        };

        // The LISTED hostname: normalization applies, the tool survives with a
        // rewritten object schema and the "Input constraint:" note.
        let listed_tools = listed(http_cfg("listed", "https://mcp.example.com/v1")).await;
        assert_eq!(
            listed_tools.len(),
            1,
            "a server whose hostname is on the flag list must have its schema NORMALIZED, not dropped: {listed_tools:?}"
        );
        assert_eq!(
            listed_tools[0].input_schema["type"],
            serde_json::json!("object")
        );
        assert!(listed_tools[0].description.starts_with("Input constraint:"));

        // An UNLISTED hostname takes the gate-off branch and is dropped.
        let other_tools = listed(http_cfg("other", "https://mcp.other.org/v1")).await;
        assert!(
            other_tools.is_empty(),
            "a server off the flag list must still be dropped: {other_tools:?}"
        );

        telemetry::test_clear_flag_list(flag);
    }

    #[tokio::test]
    async fn connect_preserves_double_underscore_tool_name() {
        let mock = Arc::new(BridgeMock::new(&["read__file"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        );
        registry.connect(cfg("fs")).await.unwrap();

        let conns = registry.connections.read().await;
        let McpConnectionState::Connected { tools, .. } = conns.get("fs").unwrap() else {
            panic!("expected Connected state");
        };
        // The `__` inside the tool name survives verbatim (normalize is a no-op
        // for names already matching `[a-zA-Z0-9_-]` — `_` is a valid char).
        assert_eq!(tools[0].full_name, "mcp__fs__read__file");
    }

    #[tokio::test]
    async fn connect_normalizes_special_char_tool_segment_and_resolves_raw_wire_name() {
        // claude-code's `buildMcpToolName` normalizes BOTH the server AND the
        // tool segment (`client.ts:1768` → `mcpStringUtils.ts:51`). A tool whose
        // wire name contains a character outside `[a-zA-Z0-9_-]` (here the `.` in
        // `weather.now`) gets a NORMALIZED model-facing FQN, while the RAW wire
        // name is kept on the dto for dispatch (claude-code's `mcpInfo.toolName`).
        let mock = Arc::new(BridgeMock::new(&["weather.now"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        );
        registry.connect(cfg("forecast")).await.unwrap();

        let conns = registry.connections.read().await;
        let McpConnectionState::Connected { tools, .. } = conns.get("forecast").unwrap() else {
            panic!("expected Connected state");
        };
        // Model-facing FQN: the `.` normalizes to `_`.
        assert_eq!(tools[0].full_name, "mcp__forecast__weather_now");
        // The dto keeps the RAW wire name for dispatch.
        assert_eq!(tools[0].tool_name, "weather.now");
        drop(conns);

        // resolve_wire_tool_name maps the normalized model-facing FQN back to
        // the RAW wire name the server expects on `tools/call`.
        assert_eq!(
            registry
                .resolve_wire_tool_name("forecast", "mcp__forecast__weather_now", None)
                .await
                .as_deref(),
            Some("weather.now"),
        );
        // Unknown FQN or server ⇒ None (the caller falls back to the parsed
        // segment, a no-op for valid-identifier names).
        assert_eq!(
            registry
                .resolve_wire_tool_name("forecast", "mcp__forecast__missing", None)
                .await,
            None,
        );
        assert_eq!(
            registry
                .resolve_wire_tool_name("nope", "mcp__forecast__weather_now", None)
                .await,
            None,
        );
    }

    #[tokio::test]
    async fn agent_scoped_connect_does_not_collide_with_a_shared_connect_of_the_same_name() {
        // §24b: a subagent's inline `mcpServers: {docs: ...}` must never clobber
        // (or be clobbered by) an unrelated shared/session-level "docs" server.
        let mock = Arc::new(BridgeMock::new(&["search"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        );
        let shared_id = registry.connect(cfg("docs")).await.unwrap();
        let (scoped_id, table_key) = registry
            .connect_agent_scoped(cfg("docs"), AgentId::new())
            .await
            .unwrap();

        assert_ne!(
            shared_id, scoped_id,
            "the scoped connect must be a SEPARATE connection, not a no-op reuse of the shared one"
        );
        assert_ne!(
            table_key, "docs",
            "the scoped table key must never equal the plain server name"
        );
        // Both entries independently readable by their OWN key; `config.name`
        // ("docs") is IDENTICAL on both — the plain display name is untouched.
        let shared_cfg = registry.get_config("docs").await.unwrap();
        assert_eq!(shared_cfg.name, "docs");
        let scoped_cfg = registry.get_config(&table_key).await.unwrap();
        assert_eq!(
            scoped_cfg.name, "docs",
            "the scoped config's plain `name` field must stay unmangled"
        );
        assert!(registry.has_callable_server("docs").await);
        assert!(registry.has_callable_server(&table_key).await);
    }

    #[tokio::test]
    async fn agent_scoped_connect_is_unreachable_by_the_plain_server_name() {
        // §24b core invariant: dispatch by the model-facing plain name must
        // NEVER accidentally resolve a private agent-scoped connection when no
        // shared server of that name exists — that would leak a subagent's
        // private server to every other caller.
        let mock = Arc::new(BridgeMock::new(&["search"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        );
        let (_id, table_key) = registry
            .connect_agent_scoped(cfg("docs"), AgentId::new())
            .await
            .unwrap();

        assert!(
            registry.get_config("docs").await.is_none(),
            "no SHARED \"docs\" server exists — the plain name must resolve to nothing"
        );
        assert!(
            !registry.has_callable_server("docs").await,
            "the plain name must not dispatch to the private scoped connection"
        );
        // The scoped key is the ONLY way to reach it.
        assert!(registry.get_config(&table_key).await.is_some());
        assert!(registry.has_callable_server(&table_key).await);
        assert!(registry.get_client(&table_key).await.is_some());
    }

    #[tokio::test]
    async fn disconnect_agent_scoped_tears_down_only_its_own_connection() {
        // §24b: tearing down a subagent's OWN newly-created connection must
        // never touch an unrelated shared connection of the same plain name.
        let mock = Arc::new(BridgeMock::new(&["search"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        );
        registry.connect(cfg("docs")).await.unwrap();
        let (_id, table_key) = registry
            .connect_agent_scoped(cfg("docs"), AgentId::new())
            .await
            .unwrap();

        registry.disconnect_agent_scoped(&table_key).await.unwrap();

        assert!(
            registry.get_config(&table_key).await.is_none(),
            "the scoped entry must be fully removed after teardown"
        );
        assert!(
            registry.has_callable_server("docs").await,
            "the UNRELATED shared \"docs\" connection must survive the scoped teardown"
        );
    }

    #[tokio::test]
    async fn two_agent_scoped_connects_of_the_same_name_get_independent_keys() {
        // §24b: TWO concurrent subagent spawns each declaring an inline
        // `mcpServers: {docs: ...}` must not collide with EACH OTHER either
        // (not just against a shared server) — this is the exact scenario the
        // reverted `agent_scope.rs` attempt mangled the name to prevent.
        let mock = Arc::new(BridgeMock::new(&["search"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        );
        let (id_a, key_a) = registry
            .connect_agent_scoped(cfg("docs"), AgentId::new())
            .await
            .unwrap();
        let (id_b, key_b) = registry
            .connect_agent_scoped(cfg("docs"), AgentId::new())
            .await
            .unwrap();

        assert_ne!(
            key_a, key_b,
            "distinct agent ids must get distinct table keys"
        );
        assert_ne!(id_a, id_b, "each spawn gets its own live connection");
        assert!(registry.get_config(&key_a).await.is_some());
        assert!(registry.get_config(&key_b).await.is_some());

        // Tearing down A must leave B fully intact.
        registry.disconnect_agent_scoped(&key_a).await.unwrap();
        assert!(registry.get_config(&key_a).await.is_none());
        assert!(
            registry.get_config(&key_b).await.is_some(),
            "spawn B's connection must survive spawn A's teardown"
        );
    }

    #[tokio::test]
    async fn connect_normalizes_claudeai_prefixed_server() {
        let mock = Arc::new(BridgeMock::new(&["search"]));
        let registry = McpRegistry::with_raw_conn(
            mock.clone() as Arc<dyn McpTransport>,
            mock as Arc<dyn RawConnectionProvider>,
        );
        // "claude.ai Linear" → normalize → "claude_ai_Linear".
        registry.connect(cfg("claude.ai Linear")).await.unwrap();

        let conns = registry.connections.read().await;
        let McpConnectionState::Connected { tools, .. } = conns.get("claude.ai Linear").unwrap()
        else {
            panic!("expected Connected state");
        };
        assert_eq!(tools[0].full_name, "mcp__claude_ai_Linear__search");
        drop(conns);
        assert!(registry.get_client("claude_ai_Linear").await.is_some());
    }

    // ---- P1-08: runtime `/add-dir` live roots + notification fan-out -------

    /// Build a paired `Connection` whose PEER ends stay observable (unlike the
    /// module `paired_connection`, which drops them), so a test can read the
    /// frames the client emits.
    fn observable_connection() -> (Arc<Connection>, mpsc::Receiver<Bytes>) {
        let (_peer_to_us_tx, peer_to_us_rx) = mpsc::channel::<Bytes>(8);
        let (us_to_peer_tx, us_to_peer_rx) = mpsc::channel::<Bytes>(8);
        let conn = Arc::new(Connection::new_streams(
            peer_to_us_rx,
            us_to_peer_tx,
            Mode::Lines,
        ));
        (conn, us_to_peer_rx)
    }

    struct PromptBridgeMock {
        conns: TestMutex<HashMap<ConnId, Arc<Connection>>>,
        inbound_txs: TestMutex<HashMap<ConnId, mpsc::Sender<Bytes>>>,
        prompt_peers: TestMutex<HashMap<ConnId, mpsc::Receiver<Bytes>>>,
        connect_calls: AtomicUsize,
        block_connect: std::sync::atomic::AtomicBool,
        connect_started: Notify,
        connect_release: Notify,
    }

    impl PromptBridgeMock {
        fn new() -> Self {
            Self {
                conns: TestMutex::new(HashMap::new()),
                inbound_txs: TestMutex::new(HashMap::new()),
                prompt_peers: TestMutex::new(HashMap::new()),
                connect_calls: AtomicUsize::new(0),
                block_connect: std::sync::atomic::AtomicBool::new(false),
                connect_started: Notify::new(),
                connect_release: Notify::new(),
            }
        }

        async fn wait_for_connection_id(&self) -> ConnId {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let connection_id = { self.conns.lock().unwrap().keys().next().copied() };
                    if let Some(connection_id) = connection_id {
                        break connection_id;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("prompt connection appears")
        }

        async fn answer_next_prompt(&self, expected_connection_id: ConnId) {
            let mut peer = self
                .prompt_peers
                .lock()
                .unwrap()
                .remove(&expected_connection_id)
                .expect("peer for connected prompt server");
            let frame = tokio::time::timeout(Duration::from_secs(2), peer.recv())
                .await
                .expect("prompts/get request within timeout")
                .expect("prompts/get frame");
            let req: Value = serde_json::from_slice(&frame).expect("json request");
            assert_eq!(req["method"], "prompts/get");
            assert_eq!(req["params"]["name"], "draft");
            assert_eq!(
                req["params"]["arguments"],
                serde_json::json!({ "topic": "release" })
            );
            let mut response = serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": req["id"].clone(),
                "result": {
                    "description": "Draft prompt",
                    "messages": [{ "role": "user", "content": { "type": "text", "text": "topic=release" } }]
                }
            }))
            .expect("serialize prompts/get response");
            response.push(b'\n');
            let sender = self
                .inbound_txs
                .lock()
                .unwrap()
                .get(&expected_connection_id)
                .expect("live inbound sender")
                .clone();
            sender
                .send(Bytes::from(response))
                .await
                .expect("send prompts/get response");
            self.prompt_peers
                .lock()
                .unwrap()
                .insert(expected_connection_id, peer);
        }
    }

    #[async_trait]
    impl McpTransport for PromptBridgeMock {
        async fn connect(&self, _s: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
            self.connect_calls.fetch_add(1, Ordering::SeqCst);
            self.connect_started.notify_one();
            if self.block_connect.load(Ordering::SeqCst) {
                self.connect_release.notified().await;
            }
            let id = ConnId::new();
            let (peer_to_us_tx, peer_to_us_rx) = mpsc::channel::<Bytes>(8);
            let (us_to_peer_tx, us_to_peer_rx) = mpsc::channel::<Bytes>(8);
            let conn = Arc::new(Connection::new_streams(
                peer_to_us_rx,
                us_to_peer_tx,
                Mode::Lines,
            ));
            self.conns.lock().unwrap().insert(id, conn);
            self.inbound_txs.lock().unwrap().insert(id, peer_to_us_tx);
            self.prompt_peers.lock().unwrap().insert(id, us_to_peer_rx);
            Ok(McpRawConnection { connection_id: id })
        }

        async fn initialize(
            &self,
            _c: &McpRawConnection,
        ) -> Result<ServerCapabilitiesDto, McpError> {
            Ok(ServerCapabilitiesDto {
                tools: false,
                resources: false,
                prompts: true,
                directory_read: false,
                logging: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            })
        }

        async fn list_tools(&self, _c: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
            Ok(Vec::new())
        }

        async fn list_resources(
            &self,
            _c: &McpRawConnection,
        ) -> Result<Vec<McpResourceDto>, McpError> {
            Ok(Vec::new())
        }

        async fn list_prompts(&self, _c: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
            Ok(vec![McpPromptDto {
                name: "draft".into(),
                description: Some("draft prompt".into()),
                arguments: Vec::new(),
            }])
        }

        async fn call_tool(
            &self,
            _c: &McpRawConnection,
            _t: &str,
            _i: Value,
        ) -> Result<McpToolResultDto, McpError> {
            unreachable!("prompt mock never serves tools")
        }

        async fn read_resource(
            &self,
            _c: &McpRawConnection,
            _u: &str,
        ) -> Result<McpResourceContentDto, McpError> {
            unreachable!("prompt mock never serves resources")
        }

        async fn ping(&self, _id: ConnId) -> Result<(), McpError> {
            Ok(())
        }

        async fn notifications(
            &self,
            _c: &McpRawConnection,
        ) -> Result<McpNotificationStream, McpError> {
            unreachable!("prompt mock notifications unused")
        }

        async fn handle_elicitation(
            &self,
            _c: &McpRawConnection,
            _r: ElicitRequestDto,
        ) -> Result<ElicitResultDto, McpError> {
            unreachable!("prompt mock elicitation unused")
        }

        async fn disconnect(&self, id: ConnId) -> Result<(), McpError> {
            self.conns.lock().unwrap().remove(&id);
            self.inbound_txs.lock().unwrap().remove(&id);
            self.prompt_peers.lock().unwrap().remove(&id);
            Ok(())
        }

        fn supported_transports(&self) -> Vec<McpTransportKind> {
            vec![McpTransportKind::Http]
        }
    }

    impl RawConnectionProvider for PromptBridgeMock {
        fn connection_for(&self, id: ConnId) -> Option<Arc<Connection>> {
            self.conns.lock().unwrap().get(&id).cloned()
        }
    }

    #[test]
    fn add_root_reports_change_only_on_a_real_add() {
        // jzn-style change-compare: a NEW dir returns true and lands in the
        // shared set; re-adding it returns false (a no-op, so the caller sends
        // NO roots/list_changed notification).
        let registry = McpRegistry::new(Arc::new(BridgeMock::new(&[])));
        assert!(
            registry.add_root(std::path::PathBuf::from("/extra")),
            "first add of a dir must report a change"
        );
        assert!(
            !registry.add_root(std::path::PathBuf::from("/extra")),
            "re-adding an already-present dir must report NO change"
        );
        assert_eq!(
            registry.additional_roots_snapshot(),
            vec![std::path::PathBuf::from("/extra")],
            "the dir is stored exactly once",
        );
    }

    #[tokio::test]
    async fn notify_roots_list_changed_all_fans_out_one_per_client() {
        // The fan-out sends exactly one `notifications/roots/list_changed` to
        // EVERY connected client (claude-code notifyMcpRootsListChanged → per-
        // client sendRootsListChanged).
        let registry = McpRegistry::new(Arc::new(BridgeMock::new(&[])));

        let (conn_a, mut peer_a) = observable_connection();
        let (conn_b, mut peer_b) = observable_connection();
        let client_a = Arc::new(McpClient::new("a", std::path::PathBuf::from("/a"), conn_a).await);
        let client_b = Arc::new(McpClient::new("b", std::path::PathBuf::from("/b"), conn_b).await);
        registry.register_test_client("a", cfg("a"), client_a).await;
        registry.register_test_client("b", cfg("b"), client_b).await;

        let notified = registry.notify_roots_list_changed_all().await;
        assert_eq!(notified, 2, "both connected clients must be notified");

        for peer in [&mut peer_a, &mut peer_b] {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(2), peer.recv())
                .await
                .expect("notification within timeout")
                .expect("a frame was emitted");
            let text = std::str::from_utf8(&frame).expect("utf-8 frame");
            assert!(
                text.contains(r#""method":"notifications/roots/list_changed""#),
                "each client must receive the roots/list_changed notification: {text}",
            );
            assert!(
                !text.contains(r#""id""#),
                "a notification carries no id: {text}"
            );
            // Exactly ONE frame per client — no second notification queued.
            assert!(
                peer.try_recv().is_err(),
                "a client must receive exactly one notification",
            );
        }
    }

    #[tokio::test]
    async fn notify_roots_list_changed_all_on_empty_registry_notifies_none() {
        let registry = McpRegistry::new(Arc::new(BridgeMock::new(&[])));
        assert_eq!(registry.notify_roots_list_changed_all().await, 0);
    }

    // -----------------------------------------------------------------
    // §26b delta 2: XAA single-flight. `resolve_xaa_token`/
    // `resolve_xaa_token_inner` are crate-private, so this lives here
    // (rather than in the integration suite, `mcp/tests/oauth_flow_test.rs`)
    // where it can call them directly — the public `connect()` entry point
    // already serializes per server name via `lifecycle_lock`, which would
    // mask whether the XAA-specific guard does anything at all.
    // -----------------------------------------------------------------

    struct FixedClock(std::time::SystemTime);
    impl platform_api::Clock for FixedClock {
        fn now(&self) -> std::time::SystemTime {
            self.0
        }
    }

    #[derive(Default)]
    struct XaaMemStorage {
        map: TestMutex<HashMap<(String, String), protocol::SecureStorageData>>,
    }
    #[async_trait]
    impl platform_api::SecureStorage for XaaMemStorage {
        async fn store(
            &self,
            service: &str,
            account: &str,
            data: protocol::SecureStorageData,
        ) -> Result<(), platform_api::SecureStorageError> {
            self.map
                .lock()
                .unwrap()
                .insert((service.into(), account.into()), data);
            Ok(())
        }
        async fn retrieve(
            &self,
            service: &str,
            account: &str,
        ) -> Result<Option<protocol::SecureStorageData>, platform_api::SecureStorageError> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .get(&(service.into(), account.into()))
                .cloned())
        }
        async fn delete(
            &self,
            service: &str,
            account: &str,
        ) -> Result<(), platform_api::SecureStorageError> {
            self.map
                .lock()
                .unwrap()
                .remove(&(service.into(), account.into()));
            Ok(())
        }
        async fn list(
            &self,
            service: &str,
        ) -> Result<Vec<String>, platform_api::SecureStorageError> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .keys()
                .filter(|(s, _)| s == service)
                .map(|(_, a)| a.clone())
                .collect())
        }
        fn is_encrypted(&self) -> bool {
            false
        }
        fn backend(&self) -> platform_api::SecureStorageBackend {
            platform_api::SecureStorageBackend::PlainText
        }
    }

    /// XAA config provider handing back fixed IdP+AS inputs.
    struct FixedXaaProvider;
    #[async_trait]
    impl XaaConfigProvider for FixedXaaProvider {
        async fn xaa_inputs(
            &self,
            _server_name: &str,
            _server_url: &str,
        ) -> Result<Option<XaaInputs>, McpError> {
            Ok(Some(XaaInputs {
                client_id: "as-client".into(),
                client_secret: "as-secret".into(),
                idp_client_id: "idp-client".into(),
                idp_client_secret: None,
                idp_id_token: "the-id-token".into(),
                idp_token_endpoint: "https://idp.example.com/token".into(),
            }))
        }
    }

    /// HTTP mock answering the XAA discovery/exchange legs; the AS
    /// jwt-bearer POST (the "mint the access token" step) counts its hits
    /// and sleeps briefly before completing, giving a concurrent second
    /// resolve every opportunity to race ahead if the single-flight guard
    /// is missing.
    struct GatedXaaHttp {
        exchange_calls: std::sync::atomic::AtomicUsize,
    }
    impl GatedXaaHttp {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                exchange_calls: std::sync::atomic::AtomicUsize::new(0),
            })
        }
    }
    #[async_trait]
    impl platform_api::HttpTransport for GatedXaaHttp {
        async fn request(
            &self,
            req: protocol::HttpRequest,
        ) -> Result<protocol::HttpResponse, platform_api::HttpError> {
            let url = req.url.clone();
            if url.contains("oauth-protected-resource") {
                return Ok(protocol::HttpResponse {
                    status: 200,
                    headers: vec![],
                    body: r#"{"resource":"https://mcp.example.com/v1","authorization_servers":["https://as.example.com"]}"#.into(),
                    body_bytes: Vec::new(),
                });
            }
            if url.contains("oauth-authorization-server") {
                return Ok(protocol::HttpResponse {
                    status: 200,
                    headers: vec![],
                    body: r#"{"issuer":"https://as.example.com","token_endpoint":"https://as.example.com/token","grant_types_supported":["urn:ietf:params:oauth:grant-type:jwt-bearer"]}"#.into(),
                    body_bytes: Vec::new(),
                });
            }
            if url.contains("idp.example.com/token") {
                return Ok(protocol::HttpResponse {
                    status: 200,
                    headers: vec![],
                    body: r#"{"access_token":"id-jag","issued_token_type":"urn:ietf:params:oauth:token-type:id-jag"}"#.into(),
                    body_bytes: Vec::new(),
                });
            }
            if url == "https://as.example.com/token" {
                self.exchange_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
                return Ok(protocol::HttpResponse {
                    status: 200,
                    headers: vec![],
                    body:
                        r#"{"access_token":"xaa-access","token_type":"Bearer","expires_in":3600}"#
                            .into(),
                    body_bytes: Vec::new(),
                });
            }
            Ok(protocol::HttpResponse {
                status: 404,
                headers: vec![],
                body: String::new(),
                body_bytes: Vec::new(),
            })
        }
        async fn stream_sse(
            &self,
            _req: protocol::HttpRequest,
        ) -> Result<platform_api::http::SseStream, platform_api::HttpError> {
            Err(platform_api::HttpError::InvalidRequest("unused".into()))
        }
    }

    fn xaa_unit_test_config(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            spec: McpTransportSpec::Http {
                url: "https://mcp.example.com/v1".into(),
                headers: platform_api::McpHeaders::new(),
                headers_helper: None,
                oauth: Some(platform_api::McpOAuthConfigDto {
                    client_id: Some("as-client".into()),
                    callback_port: None,
                    auth_server_metadata_url: None,
                    scopes: None,
                    xaa: Some(true),
                }),
            },
            scope: ConfigScope::Settings(protocol::SettingsScope::Project),
            disabled: false,
            timeout_ms: None,
            always_load: false,
            discovery_cache: None,
            tools: Vec::new(),
            tool_permissions: std::collections::BTreeMap::new(),
            config_error: None,
            metadata: Default::default(),
        }
    }

    /// §26b delta 2: two concurrent `resolve_xaa_token` calls for the SAME
    /// server key must share one exchange. Without the `xaa_refresh_lock`
    /// guard, task B (spawned once task A's exchange is confirmed in
    /// flight, and given a 50ms window while A "sleeps" mid-request) would
    /// independently run its own full IdP+AS chain and land on the same AS
    /// jwt-bearer endpoint too, driving `exchange_calls` to 2.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn xaa_concurrent_resolves_share_one_exchange() {
        let _capture = test_telemetry_capture_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_test_telemetry_events();
        std::env::set_var("LINGXI_ENABLE_XAA", "1");

        let http = GatedXaaHttp::new();
        let registry = Arc::new(McpRegistry::new(Arc::new(BridgeMock::new(&[]))).with_oauth(
            OAuthDeps {
                http: http.clone() as Arc<dyn platform_api::HttpTransport>,
                clock: Arc::new(FixedClock(
                    std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
                )),
                storage: Arc::new(XaaMemStorage::default()) as Arc<dyn platform_api::SecureStorage>,
                on_authorization_url: Arc::new(|_url: &str| {}),
                xaa_config: Some(Arc::new(FixedXaaProvider)),
            },
        ));

        let config = xaa_unit_test_config("xaa-concurrent");
        let key = oauth::server_key(&config.name, &config.spec);

        let (r1, c1, k1) = (registry.clone(), config.clone(), key.clone());
        let task_a = tokio::spawn(async move {
            let deps = r1.oauth.as_ref().unwrap().clone();
            r1.resolve_xaa_token(&c1, &k1, &deps).await
        });
        let (r2, c2, k2) = (registry.clone(), config.clone(), key.clone());
        let task_b = tokio::spawn(async move {
            let deps = r2.oauth.as_ref().unwrap().clone();
            r2.resolve_xaa_token(&c2, &k2, &deps).await
        });

        let (res_a, res_b) = tokio::join!(task_a, task_b);
        std::env::remove_var("LINGXI_ENABLE_XAA");

        let tok_a = res_a.unwrap().expect("task A resolves");
        let tok_b = res_b.unwrap().expect("task B resolves");
        assert_eq!(tok_a.access_token.expose_secret(), "xaa-access");
        assert_eq!(tok_b.access_token.expose_secret(), "xaa-access");
        assert_eq!(
            http.exchange_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "single-flight: exactly one AS jwt-bearer exchange for two concurrent resolves"
        );
        let success_events: Vec<_> = take_test_telemetry_events()
            .into_iter()
            .filter(|event| event.name == telemetry::tengu::mcp::OAUTH_FLOW_SUCCESS)
            .collect();
        assert_eq!(success_events.len(), 1);
        assert_eq!(
            success_events[0].payload,
            serde_json::json!({"authMethod":"xaa","idTokenCacheHit":false})
        );
    }

    #[tokio::test]
    async fn xaa_issuer_mismatch_is_reported_as_discovery_failure() {
        let _capture = test_telemetry_capture_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_test_telemetry_events();
        std::env::set_var("LINGXI_ENABLE_XAA", "1");

        struct IssuerMismatchHttp;
        #[async_trait]
        impl platform_api::HttpTransport for IssuerMismatchHttp {
            async fn request(
                &self,
                req: protocol::HttpRequest,
            ) -> Result<protocol::HttpResponse, platform_api::HttpError> {
                let (status, body) = if req.url.contains("oauth-protected-resource") {
                    (
                        200,
                        r#"{"resource":"https://mcp.example.com/v1","authorization_servers":["https://as.example.com/root"]}"#.to_string(),
                    )
                } else if req.url.contains("oauth-authorization-server") {
                    (
                        200,
                        r#"{"issuer":"https://other.example.com/root","token_endpoint":"https://other.example.com/token","grant_types_supported":["urn:ietf:params:oauth:grant-type:jwt-bearer"]}"#.to_string(),
                    )
                } else {
                    (404, String::new())
                };
                Ok(protocol::HttpResponse {
                    status,
                    headers: vec![],
                    body,
                    body_bytes: Vec::new(),
                })
            }
            async fn stream_sse(
                &self,
                _req: protocol::HttpRequest,
            ) -> Result<platform_api::http::SseStream, platform_api::HttpError> {
                Err(platform_api::HttpError::InvalidRequest("unused".into()))
            }
        }

        let registry = McpRegistry::new(Arc::new(BridgeMock::new(&[]))).with_oauth(OAuthDeps {
            http: Arc::new(IssuerMismatchHttp),
            clock: Arc::new(FixedClock(
                std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
            )),
            storage: Arc::new(XaaMemStorage::default()) as Arc<dyn platform_api::SecureStorage>,
            on_authorization_url: Arc::new(|_url: &str| {}),
            xaa_config: Some(Arc::new(FixedXaaProvider)),
        });

        let config = xaa_unit_test_config("xaa-issuer");
        let key = oauth::server_key(&config.name, &config.spec);
        let deps = registry.oauth.as_ref().unwrap().clone();
        let error = registry
            .resolve_xaa_token(&config, &key, &deps)
            .await
            .expect_err("issuer mismatch must fail");
        std::env::remove_var("LINGXI_ENABLE_XAA");

        assert!(
            matches!(error, McpError::OAuth(_)),
            "unexpected error: {error:?}"
        );
        let events = take_test_telemetry_events();
        let event = events
            .iter()
            .find(|event| event.name == telemetry::tengu::mcp::OAUTH_FLOW_FAILURE)
            .expect("XAA flow failure telemetry");
        assert_eq!(event.payload["authMethod"], serde_json::json!("xaa"));
        assert_eq!(
            event.payload["xaaFailureStage"],
            serde_json::json!("discovery")
        );
        assert_eq!(event.payload["idTokenCacheHit"], serde_json::json!(false));
        assert!(events
            .iter()
            .all(|event| event.name != telemetry::tengu::mcp::OAUTH_ISSUER_ECHO_MISMATCH));
    }

    #[test]
    fn xaa_prm_failure_is_classified_as_discovery_without_error_detail() {
        let error = crate::xaa::XaaError::Prm("secret-bearing transport detail".to_string());

        assert_eq!(xaa_flow_failure_stage(&error), "discovery");
    }

    #[tokio::test]
    async fn xaa_jwt_bearer_failure_emits_oauth_flow_failure() {
        let _capture = test_telemetry_capture_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_test_telemetry_events();
        std::env::set_var("LINGXI_ENABLE_XAA", "1");

        struct CacheHitXaaProvider;
        #[async_trait]
        impl XaaConfigProvider for CacheHitXaaProvider {
            async fn xaa_inputs(
                &self,
                _server_name: &str,
                _server_url: &str,
            ) -> Result<Option<XaaInputs>, McpError> {
                Ok(Some(XaaInputs {
                    client_id: "as-client".into(),
                    client_secret: "as-secret".into(),
                    idp_client_id: "idp-client".into(),
                    idp_client_secret: None,
                    idp_id_token: "the-id-token".into(),
                    idp_token_endpoint: "https://idp.example.com/token".into(),
                }))
            }

            async fn peek_id_token_cache_hit(
                &self,
                _server_name: &str,
                _server_url: &str,
            ) -> Result<bool, McpError> {
                Ok(true)
            }
        }

        struct JwtBearerFailureHttp;
        #[async_trait]
        impl platform_api::HttpTransport for JwtBearerFailureHttp {
            async fn request(
                &self,
                req: protocol::HttpRequest,
            ) -> Result<protocol::HttpResponse, platform_api::HttpError> {
                let (status, body) = if req.url.contains("oauth-protected-resource") {
                    (
                        200,
                        r#"{"resource":"https://mcp.example.com/v1","authorization_servers":["https://as.example.com/root"]}"#.to_string(),
                    )
                } else if req.url.contains("oauth-authorization-server") {
                    (
                        200,
                        r#"{"issuer":"https://as.example.com/root","token_endpoint":"https://as.example.com/token","grant_types_supported":["urn:ietf:params:oauth:grant-type:jwt-bearer"]}"#.to_string(),
                    )
                } else if req.url.contains("idp.example.com/token") {
                    (
                        200,
                        "{\"access_token\":\"idp-access\",\"issued_token_type\":\"urn:ietf:params:oauth:token-type:id-jag\",\"token_type\":\"Bearer\",\"expires_in\":3600}".to_string(),
                    )
                } else if req.url.contains("as.example.com/token") {
                    (400, r#"{"error":"invalid_grant"}"#.to_string())
                } else {
                    (404, String::new())
                };
                Ok(protocol::HttpResponse {
                    status,
                    headers: vec![],
                    body,
                    body_bytes: Vec::new(),
                })
            }

            async fn stream_sse(
                &self,
                _req: protocol::HttpRequest,
            ) -> Result<platform_api::http::SseStream, platform_api::HttpError> {
                Err(platform_api::HttpError::InvalidRequest("unused".into()))
            }
        }

        let registry = McpRegistry::new(Arc::new(BridgeMock::new(&[]))).with_oauth(OAuthDeps {
            http: Arc::new(JwtBearerFailureHttp),
            clock: Arc::new(FixedClock(
                std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
            )),
            storage: Arc::new(XaaMemStorage::default()) as Arc<dyn platform_api::SecureStorage>,
            on_authorization_url: Arc::new(|_url: &str| {}),
            xaa_config: Some(Arc::new(CacheHitXaaProvider)),
        });

        let config = xaa_unit_test_config("xaa-jwt-bearer");
        let key = oauth::server_key(&config.name, &config.spec);
        let deps = registry.oauth.as_ref().unwrap().clone();
        let error = registry
            .resolve_xaa_token(&config, &key, &deps)
            .await
            .expect_err("jwt-bearer failure must fail");
        std::env::remove_var("LINGXI_ENABLE_XAA");

        assert!(matches!(error, McpError::OAuth(_)));
        let events = take_test_telemetry_events();
        let event = events
            .iter()
            .find(|event| event.name == telemetry::tengu::mcp::OAUTH_FLOW_FAILURE)
            .expect("oauth flow failure telemetry");
        assert_eq!(event.payload["authMethod"], serde_json::json!("xaa"));
        assert_eq!(
            event.payload["xaaFailureStage"],
            serde_json::json!("jwt_bearer")
        );
        assert_eq!(event.payload["idTokenCacheHit"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn xaa_provider_discovery_failure_emits_oauth_flow_failure() {
        let _capture = test_telemetry_capture_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        clear_test_telemetry_events();
        std::env::set_var("LINGXI_ENABLE_XAA", "1");

        struct DiscoveryFailingXaaProvider;
        #[async_trait]
        impl XaaConfigProvider for DiscoveryFailingXaaProvider {
            async fn xaa_inputs(
                &self,
                _server_name: &str,
                _server_url: &str,
            ) -> Result<Option<XaaInputs>, McpError> {
                Err(McpError::OAuth(
                    "XAA IdP: OIDC discovery transport: timeout".into(),
                ))
            }

            async fn peek_id_token_cache_hit(
                &self,
                _server_name: &str,
                _server_url: &str,
            ) -> Result<bool, McpError> {
                Ok(true)
            }
        }

        struct UnusedHttp;
        #[async_trait]
        impl platform_api::HttpTransport for UnusedHttp {
            async fn request(
                &self,
                _req: protocol::HttpRequest,
            ) -> Result<protocol::HttpResponse, platform_api::HttpError> {
                Err(platform_api::HttpError::InvalidRequest("unused".into()))
            }

            async fn stream_sse(
                &self,
                _req: protocol::HttpRequest,
            ) -> Result<platform_api::http::SseStream, platform_api::HttpError> {
                Err(platform_api::HttpError::InvalidRequest("unused".into()))
            }
        }

        let registry = McpRegistry::new(Arc::new(BridgeMock::new(&[]))).with_oauth(OAuthDeps {
            http: Arc::new(UnusedHttp),
            clock: Arc::new(FixedClock(
                std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
            )),
            storage: Arc::new(XaaMemStorage::default()) as Arc<dyn platform_api::SecureStorage>,
            on_authorization_url: Arc::new(|_url: &str| {}),
            xaa_config: Some(Arc::new(DiscoveryFailingXaaProvider)),
        });

        let config = xaa_unit_test_config("xaa-provider-discovery");
        let key = oauth::server_key(&config.name, &config.spec);
        let deps = registry.oauth.as_ref().unwrap().clone();
        let error = registry
            .resolve_xaa_token(&config, &key, &deps)
            .await
            .expect_err("provider discovery failure must fail");
        std::env::remove_var("LINGXI_ENABLE_XAA");

        assert!(matches!(error, McpError::OAuth(_)));
        let events = take_test_telemetry_events();
        let event = events
            .iter()
            .find(|event| event.name == telemetry::tengu::mcp::OAUTH_FLOW_FAILURE)
            .expect("oauth flow failure telemetry");
        assert_eq!(event.payload["authMethod"], serde_json::json!("xaa"));
        assert_eq!(
            event.payload["xaaFailureStage"],
            serde_json::json!("discovery")
        );
        assert_eq!(event.payload["idTokenCacheHit"], serde_json::json!(true));
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use crate::connection::{ConfigScope, McpServerConfig};
    use async_trait::async_trait;
    use platform_api::{
        ElicitRequestDto, ElicitResultDto, McpError, McpNotificationStream, McpPromptDto,
        McpRawConnection, McpResourceContentDto, McpResourceDto, McpServerInfo, McpStatus,
        McpToolDto, McpToolResultDto, McpTransport, McpTransportKind, McpTransportSpec,
        ServerCapabilitiesDto,
    };
    use protocol::McpConnectionId as ConnId;
    use serde_json::Value;
    use std::sync::Arc;

    /// Minimal in-crate stub transport — only `connections` matters for
    /// `snapshot`, so every method panics if called.
    struct StubTransport;

    #[async_trait]
    impl McpTransport for StubTransport {
        async fn connect(&self, _s: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
            unreachable!()
        }
        async fn initialize(
            &self,
            _c: &McpRawConnection,
        ) -> Result<ServerCapabilitiesDto, McpError> {
            unreachable!()
        }
        async fn list_tools(&self, _c: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
            unreachable!()
        }
        async fn list_resources(
            &self,
            _c: &McpRawConnection,
        ) -> Result<Vec<McpResourceDto>, McpError> {
            unreachable!()
        }
        async fn list_prompts(&self, _c: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
            unreachable!()
        }
        async fn call_tool(
            &self,
            _c: &McpRawConnection,
            _t: &str,
            _i: Value,
        ) -> Result<McpToolResultDto, McpError> {
            unreachable!()
        }
        async fn read_resource(
            &self,
            _c: &McpRawConnection,
            _u: &str,
        ) -> Result<McpResourceContentDto, McpError> {
            unreachable!()
        }
        async fn ping(&self, _id: ConnId) -> Result<(), McpError> {
            unreachable!()
        }
        async fn notifications(
            &self,
            _c: &McpRawConnection,
        ) -> Result<McpNotificationStream, McpError> {
            unreachable!()
        }
        async fn handle_elicitation(
            &self,
            _c: &McpRawConnection,
            _r: ElicitRequestDto,
        ) -> Result<ElicitResultDto, McpError> {
            unreachable!()
        }
        async fn disconnect(&self, _id: ConnId) -> Result<(), McpError> {
            unreachable!()
        }
        fn supported_transports(&self) -> Vec<McpTransportKind> {
            vec![McpTransportKind::Stdio]
        }
    }

    fn stdio_cfg(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.into(),
            spec: McpTransportSpec::Stdio {
                command: "echo".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
            },
            scope: ConfigScope::Settings(protocol::SettingsScope::Project),
            disabled: false,
            timeout_ms: None,
            always_load: false,
            discovery_cache: None,
            tools: Vec::new(),
            tool_permissions: std::collections::BTreeMap::new(),
            config_error: None,
            metadata: Default::default(),
        }
    }

    #[test]
    fn server_connection_payloads_only_mark_plugin_agent_source_as_plugin() {
        let mut plugin_cfg = stdio_cfg("plugin:demo:srv");
        plugin_cfg.scope = ConfigScope::Dynamic;
        plugin_cfg.metadata.agent_source = Some(crate::connection::McpAgentSource::Plugin);
        let plugin_succeeded = server_connection_succeeded_payload(
            &plugin_cfg,
            12,
            crate::protocol_negotiation::NegotiationMode::Legacy,
            &platform_api::McpNegotiatedProtocol {
                era: platform_api::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
        );
        let plugin_failed = server_connection_failed_payload(
            &plugin_cfg,
            Some(crate::protocol_negotiation::NegotiationMode::Legacy),
            Some(12),
            Some("INVALID_CONFIG"),
        );
        assert!(plugin_succeeded.is_plugin);
        assert!(plugin_failed.is_plugin);

        let dynamic_cfg = McpServerConfig {
            scope: ConfigScope::Dynamic,
            ..stdio_cfg("dynamic")
        };
        let dynamic_succeeded = server_connection_succeeded_payload(
            &dynamic_cfg,
            8,
            crate::protocol_negotiation::NegotiationMode::Legacy,
            &platform_api::McpNegotiatedProtocol {
                era: platform_api::McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
        );
        let dynamic_failed = server_connection_failed_payload(
            &dynamic_cfg,
            Some(crate::protocol_negotiation::NegotiationMode::Legacy),
            Some(8),
            Some("INVALID_CONFIG"),
        );
        assert!(!dynamic_succeeded.is_plugin);
        assert!(!dynamic_failed.is_plugin);
    }

    #[tokio::test]
    async fn snapshot_empty_registry() {
        let r = McpRegistry::new(Arc::new(StubTransport));
        assert_eq!(r.snapshot().await, Vec::<McpServerInfo>::new());
    }

    #[tokio::test]
    async fn unconfigured_and_invalid_config_short_circuit_before_dialing() {
        // `StubTransport::connect` is `unreachable!()`, so reaching the dial
        // panics — both of `Nxe`'s pre-dial gates have to fire here, and they
        // carry DIFFERENT oracle error codes (UNCONFIGURED vs INVALID_CONFIG)
        // that `mcp list`/`mcp get` render differently.
        let r = McpRegistry::new(Arc::new(StubTransport));

        let mut blank = stdio_cfg("blank");
        blank.spec = McpTransportSpec::Http {
            url: "   ".into(),
            headers: platform_api::McpHeaders::default(),
            headers_helper: None,
            oauth: None,
        };
        assert!(
            blank.is_unconfigured(),
            "blank url + no configError = `zar`"
        );
        assert_eq!(
            r.connect(blank).await.unwrap_err().to_string(),
            "connection failed: No URL configured for this server"
        );

        let mut broken = stdio_cfg("broken");
        broken.spec = McpTransportSpec::Http {
            url: "${MISSING:-}".into(),
            headers: platform_api::McpHeaders::default(),
            headers_helper: None,
            oauth: None,
        };
        broken.config_error =
            Some("'url' \"${MISSING:-}\" expanded to an empty string.".to_string());
        assert!(
            !broken.is_unconfigured(),
            "`url_invalid` is INVALID_CONFIG, never UNCONFIGURED"
        );
        assert_eq!(
            r.connect(broken).await.unwrap_err().to_string(),
            "connection failed: 'url' \"${MISSING:-}\" expanded to an empty string."
        );

        // §18 — the oracle's CONNECT-TIME `new URL(t.url)` re-check
        // (`Ve`/`Ae` @182283839 / @182488092), which fires on a url that is
        // present and non-blank but does not parse. Nothing at load time
        // records a `config_error` for it, so before this gate existed the
        // registry dialed a garbage url and surfaced a raw transport error.
        // `StubTransport::connect` is `unreachable!()`, so reaching the dial
        // panics the test.
        let mut malformed = stdio_cfg("malformed");
        malformed.spec = McpTransportSpec::Http {
            url: "api.example.com/mcp".into(),
            headers: platform_api::McpHeaders::default(),
            headers_helper: None,
            oauth: None,
        };
        assert!(
            !malformed.is_unconfigured(),
            "a non-blank url is never UNCONFIGURED"
        );
        assert_eq!(
            r.connect(malformed).await.unwrap_err().to_string(),
            "connection failed: 'url' is not a valid URL. Update the server's config and reconnect."
        );
    }

    #[tokio::test]
    async fn servers_with_tools_empty_when_no_clients() {
        // No registered clients (e.g. a server still connecting / awaiting OAuth
        // exposes no tools) → empty. Used by AgentTool's required-MCP gate.
        let r = McpRegistry::new(Arc::new(StubTransport));
        assert!(r.servers_with_tools().await.is_empty());
    }

    #[tokio::test]
    async fn servers_pending_and_failed_classify_internal_states() {
        // The required-MCP poll-wait needs to distinguish a still-connecting
        // server from a failed/absent one — the public `McpStatus` projection
        // collapses these, so `servers_pending`/`servers_failed` read the
        // internal state map. `pending` = Connecting | AwaitingOAuth |
        // Reconnecting; `failed` = Failed. Connected/Disconnected/Stopped are
        // neither.
        let r = McpRegistry::new(Arc::new(StubTransport));
        {
            let mut c = r.connections.write().await;
            c.insert(
                "connecting".into(),
                McpConnectionState::Connecting {
                    config: stdio_cfg("connecting"),
                    started_at: SystemTime::now(),
                },
            );
            c.insert(
                "awaiting".into(),
                McpConnectionState::AwaitingOAuth {
                    config: stdio_cfg("awaiting"),
                    callback_port: 7777,
                },
            );
            c.insert(
                "reconnecting".into(),
                McpConnectionState::Reconnecting {
                    config: stdio_cfg("reconnecting"),
                    retry_count: 2,
                    next_retry_at: SystemTime::now(),
                },
            );
            c.insert(
                "boom".into(),
                McpConnectionState::Failed {
                    config: stdio_cfg("boom"),
                    error: "nope".into(),
                    attempts: 5,
                },
            );
            c.insert(
                "idle".into(),
                McpConnectionState::Disconnected {
                    config: stdio_cfg("idle"),
                    last_error: None,
                },
            );
        }
        let mut pending = r.servers_pending().await;
        pending.sort();
        assert_eq!(pending, vec!["awaiting", "connecting", "reconnecting"]);
        assert_eq!(r.servers_failed().await, vec!["boom".to_string()]);
    }

    #[tokio::test]
    async fn snapshot_disconnected_server_no_error() {
        let r = McpRegistry::new(Arc::new(StubTransport));
        r.connections.write().await.insert(
            "memory".into(),
            McpConnectionState::Disconnected {
                config: stdio_cfg("memory"),
                last_error: None,
            },
        );
        let snap = r.snapshot().await;
        assert_eq!(
            snap,
            vec![McpServerInfo {
                name: "memory".into(),
                status: McpStatus::Disconnected,
                transport: "stdio".into(),
            }]
        );
    }

    #[tokio::test]
    async fn snapshot_disconnected_with_error_becomes_error_status() {
        let r = McpRegistry::new(Arc::new(StubTransport));
        r.connections.write().await.insert(
            "memory".into(),
            McpConnectionState::Disconnected {
                config: stdio_cfg("memory"),
                last_error: Some("boom".into()),
            },
        );
        let snap = r.snapshot().await;
        assert_eq!(snap[0].status, McpStatus::Error("boom".into()));
    }

    #[tokio::test]
    async fn snapshot_sorts_by_name() {
        let r = McpRegistry::new(Arc::new(StubTransport));
        {
            let mut c = r.connections.write().await;
            c.insert(
                "memory".into(),
                McpConnectionState::Disconnected {
                    config: stdio_cfg("memory"),
                    last_error: None,
                },
            );
            c.insert(
                "filesystem".into(),
                McpConnectionState::Disconnected {
                    config: stdio_cfg("filesystem"),
                    last_error: None,
                },
            );
        }
        let snap = r.snapshot().await;
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].name, "filesystem");
        assert_eq!(snap[1].name, "memory");
    }

    #[test]
    fn conversation_export_identity_preserves_hyphens_and_split_boundaries() {
        let scope = ConversationExport::new("abc--1", "0".repeat(64)).unwrap();
        assert_eq!(scope.server_name(), "local_app_abc--1");
        assert_eq!(scope.server_info_name(), "lingxi-local-app");
        assert_eq!(
            scope.registry_key(),
            "local_apps:conversation-export:abc--1"
        );
        assert_eq!(
            scope.tool_full_name("read_value").unwrap(),
            "mcp__local_app_abc--1__read_value"
        );
        assert!(ConversationExport::new("abc_1", "0".repeat(64)).is_err());
        assert!(scope.tool_full_name("bad__name").is_err());
    }

    #[test]
    fn conversation_export_uses_schema_v3_app_id_boundaries() {
        let id_54 = format!("a{}", "b".repeat(53));
        let id_55 = format!("a{}", "b".repeat(54));
        assert!(ConversationExport::new(id_54, "0".repeat(64)).is_ok());
        assert!(ConversationExport::new(id_55, "0".repeat(64)).is_err());
        assert!(ConversationExport::new("A123", "0".repeat(64)).is_err());
        assert!(ConversationExport::new("-leading", "0".repeat(64)).is_err());
    }

    #[tokio::test]
    async fn managed_local_apps_share_one_physical_hub_and_notify_changed_catalog_partitions() {
        let registry = McpRegistry::new(Arc::new(StubTransport));
        let mut events = registry.subscribe_catalog_changes();
        for index in 0..100 {
            let scope = ConversationExport::new(format!("app-{index}"), "0".repeat(64)).unwrap();
            registry
                .register_managed_local_app(scope, "1".repeat(64), true)
                .await
                .unwrap();
            let change = events.recv().await.unwrap();
            assert_eq!(change.server_name, format!("local_app_app-{index}"));
        }
        assert_eq!(registry.managed_local_app_count().await, 100);
        assert_eq!(registry.physical_transport_count(), 1);
        let scope = ConversationExport::new("app-0", "0".repeat(64)).unwrap();
        registry
            .register_managed_local_app(scope, "2".repeat(64), false)
            .await
            .unwrap();
        assert_eq!(events.recv().await.unwrap().kind, McpCatalogKind::Resources);
        // A bad `surface_changed=true` hint cannot duplicate the event when
        // the committed surface digest is unchanged.
        let same_surface = ConversationExport::new("app-0", "0".repeat(64)).unwrap();
        let refreshed = registry
            .register_managed_local_app(same_surface, "3".repeat(64), true)
            .await
            .unwrap();
        assert_eq!(refreshed.surface_generation, 1);
        assert_eq!(events.recv().await.unwrap().kind, McpCatalogKind::Resources);
        let changed_surface = ConversationExport::new("app-0", "f".repeat(64)).unwrap();
        let changed = registry
            .register_managed_local_app(changed_surface, "4".repeat(64), false)
            .await
            .unwrap();
        assert_eq!(changed.surface_generation, 2);
        let tools = events.recv().await.unwrap();
        assert_eq!(tools.server_name, "local_app_app-0");
        assert_eq!(tools.kind, McpCatalogKind::Tools);
        assert_eq!(events.recv().await.unwrap().kind, McpCatalogKind::Resources);
        assert!(registry
            .unregister_managed_local_app("app-0")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn managed_local_app_catalog_refresh_notifies_resources_only() {
        let registry = McpRegistry::new(Arc::new(StubTransport));
        let mut events = registry.subscribe_catalog_changes();
        let scope = ConversationExport::new("app-0", "0".repeat(64)).unwrap();
        registry
            .register_managed_local_app(scope.clone(), "1".repeat(64), false)
            .await
            .unwrap();
        assert_eq!(events.recv().await.unwrap().kind, McpCatalogKind::Tools);
        registry
            .register_managed_local_app(scope, "2".repeat(64), false)
            .await
            .unwrap();
        let event = events.recv().await.unwrap();
        assert_eq!(event.server_name, "local_app_app-0");
        assert_eq!(event.kind, McpCatalogKind::Resources);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), events.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn local_app_exposure_is_bounded_lru_and_pin_aware() {
        let registry = McpRegistry::new(Arc::new(StubTransport));
        for index in 0..10 {
            let scope = ConversationExport::new(format!("app-{index}"), "0".repeat(64)).unwrap();
            registry
                .register_managed_local_app(scope, "1".repeat(64), false)
                .await
                .unwrap();
        }

        assert!(registry
            .local_app_exposures("conversation")
            .await
            .is_empty());
        for index in 0..8 {
            registry
                .expose_managed_local_app("conversation", &format!("app-{index}"), false)
                .await
                .unwrap();
        }
        assert_eq!(registry.local_app_exposures("conversation").await.len(), 8);

        // app-0 is the oldest unpinned entry and is the only one evicted.
        registry
            .expose_managed_local_app("conversation", "app-8", false)
            .await
            .unwrap();
        let ids: Vec<String> = registry
            .local_app_exposures("conversation")
            .await
            .into_iter()
            .map(|entry| entry.app_id)
            .collect();
        assert!(!ids.iter().any(|id| id == "app-0"));
        assert!(ids.iter().any(|id| id == "app-8"));

        // Pinning is explicit. The next eviction skips app-1 even though it
        // is older than the unpinned entries.
        registry
            .pin_local_app_exposure("conversation", "app-1", true)
            .await
            .unwrap();
        registry
            .expose_managed_local_app("conversation", "app-9", false)
            .await
            .unwrap();
        let ids: Vec<String> = registry
            .local_app_exposures("conversation")
            .await
            .into_iter()
            .map(|entry| entry.app_id)
            .collect();
        assert!(ids.iter().any(|id| id == "app-1"));
        assert!(ids.iter().any(|id| id == "app-9"));
    }

    #[tokio::test]
    async fn local_app_exposure_rejects_ninth_when_all_are_pinned_and_tracks_calls() {
        let registry = McpRegistry::new(Arc::new(StubTransport));
        for index in 0..9 {
            let scope = ConversationExport::new(format!("pin-{index}"), "0".repeat(64)).unwrap();
            registry
                .register_managed_local_app(scope, "1".repeat(64), false)
                .await
                .unwrap();
        }
        for index in 0..8 {
            registry
                .expose_managed_local_app("conversation", &format!("pin-{index}"), true)
                .await
                .unwrap();
        }
        let err = registry
            .expose_managed_local_app("conversation", "pin-8", false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("exposure_capacity_reached"));
        assert!(err.to_string().contains("pin-0"));

        for _ in 0..4 {
            registry
                .begin_local_app_call("conversation", "pin-0")
                .await
                .unwrap();
        }
        let err = registry
            .begin_local_app_call("conversation", "pin-0")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("rate_limited"));
        for _ in 0..5 {
            registry.end_local_app_call("conversation", "pin-0").await;
        }
        assert_eq!(
            registry
                .local_app_exposures("conversation")
                .await
                .iter()
                .find(|entry| entry.app_id == "pin-0")
                .unwrap()
                .in_flight,
            0
        );
    }

    #[tokio::test]
    async fn local_app_exposure_never_evicts_an_inflight_entry() {
        let registry = McpRegistry::new(Arc::new(StubTransport));
        for index in 0..9 {
            let scope = ConversationExport::new(format!("busy-{index}"), "0".repeat(64)).unwrap();
            registry
                .register_managed_local_app(scope, "1".repeat(64), false)
                .await
                .unwrap();
        }
        for index in 0..8 {
            registry
                .expose_managed_local_app("conversation", &format!("busy-{index}"), false)
                .await
                .unwrap();
        }
        registry
            .begin_local_app_call("conversation", "busy-0")
            .await
            .unwrap();
        registry
            .expose_managed_local_app("conversation", "busy-8", false)
            .await
            .unwrap();
        let exposed = registry.local_app_exposures("conversation").await;
        assert!(exposed.iter().any(|entry| entry.app_id == "busy-0"));
        assert!(exposed.iter().any(|entry| entry.app_id == "busy-8"));
        assert_eq!(
            exposed
                .iter()
                .find(|entry| entry.app_id == "busy-0")
                .unwrap()
                .in_flight,
            1
        );
        registry.end_local_app_call("conversation", "busy-0").await;
    }

    #[tokio::test]
    async fn deleting_managed_local_app_removes_all_conversation_exposure() {
        let registry = McpRegistry::new(Arc::new(StubTransport));
        let scope = ConversationExport::new("delete-me", "0".repeat(64)).unwrap();
        registry
            .register_managed_local_app(scope, "1".repeat(64), false)
            .await
            .unwrap();
        registry
            .expose_managed_local_app("conversation", "delete-me", true)
            .await
            .unwrap();
        assert!(registry
            .unregister_managed_local_app("delete-me")
            .await
            .unwrap());
        assert!(registry
            .local_app_exposures("conversation")
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn disabling_managed_local_app_clears_exposure_and_emits_changes() {
        let registry = McpRegistry::new(Arc::new(StubTransport));
        let mut events = registry.subscribe_catalog_changes();
        registry
            .register_managed_local_app(
                ConversationExport::new("toggle-me", "0".repeat(64)).unwrap(),
                "1".repeat(64),
                false,
            )
            .await
            .unwrap();
        let _ = events.recv().await.unwrap();
        registry
            .expose_managed_local_app("conversation", "toggle-me", true)
            .await
            .unwrap();
        let runtime = registry
            .set_managed_local_app_runtime(
                "toggle-me",
                false,
                Some(vec!["read_value".into()]),
                Some(ManagedLocalAppResource {
                    uri: "ui://local-app/toggle-me/0fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff/mcp-app.html".into(),
                    name: "Toggle".into(),
                    description: None,
                    mime_type: Some("text/html;profile=mcp-app".into()),
                    meta: None,
                }),
            )
            .await
            .unwrap();
        assert!(!runtime.enabled);
        assert!(registry
            .local_app_exposures("conversation")
            .await
            .is_empty());
        assert_eq!(events.recv().await.unwrap().kind, McpCatalogKind::Tools);
        assert_eq!(events.recv().await.unwrap().kind, McpCatalogKind::Resources);
        let err = registry
            .expose_managed_local_app("conversation", "toggle-me", false)
            .await
            .unwrap_err();
        assert!(matches!(err, McpError::ToolNotFound(_)));
    }

    #[tokio::test]
    async fn expose_managed_local_app_reports_the_evicted_entry() {
        let registry = McpRegistry::new(Arc::new(StubTransport));
        for index in 0..9 {
            registry
                .register_managed_local_app(
                    ConversationExport::new(format!("diff-{index}"), "0".repeat(64)).unwrap(),
                    "1".repeat(64),
                    false,
                )
                .await
                .unwrap();
        }
        for index in 0..8 {
            registry
                .expose_managed_local_app("conversation", &format!("diff-{index}"), false)
                .await
                .unwrap();
        }
        let update = registry
            .expose_managed_local_app_with_diff("conversation", "diff-8", false)
            .await
            .unwrap();
        assert_eq!(update.exposure.app_id, "diff-8");
        assert_eq!(update.evicted_app_id.as_deref(), Some("diff-0"));
    }

    #[tokio::test]
    async fn managed_local_apps_snapshot_is_sorted() {
        let registry = McpRegistry::new(Arc::new(StubTransport));
        for app_id in ["b-app", "a-app"] {
            registry
                .register_managed_local_app(
                    ConversationExport::new(app_id, "0".repeat(64)).unwrap(),
                    "1".repeat(64),
                    false,
                )
                .await
                .unwrap();
        }
        let apps = registry.managed_local_apps().await;
        assert_eq!(
            apps.iter()
                .map(|server| server.scope.app_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a-app", "b-app"]
        );
    }
}
