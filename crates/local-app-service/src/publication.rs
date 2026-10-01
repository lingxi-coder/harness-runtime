//! Making an app's approved MCP tools available to a conversation.
//!
//! An app can carry its own MCP server: a catalog of tools the person approved,
//! a subset of which they turned on. How those tools reach a model is the
//! host's business (the engine keeps a registry of logical servers, per
//! conversation, with a budget of how many may be live at once). The service
//! decides *what* is publishable and says so through [`McpPublisher`]; it never
//! touches the registry.

use async_trait::async_trait;
use serde_json::Value;

/// One app's MCP surface, as the service knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedApp {
    /// The app.
    pub app_id: String,
    /// Digest of the approved catalog the tools come from.
    pub catalog_sha256: String,
    /// Digest of the approved tool surface narrowed to the tools the person has
    /// enabled. It changes whenever what a model can see changes, so a host can
    /// tell a stale exposure from a current one by comparing it.
    pub effective_surface_sha256: String,
}

/// A widget the app's tools ask the client to render beside their results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WidgetResource {
    /// Where the widget is addressed.
    pub uri: String,
    /// What it is called.
    pub name: String,
    /// What it is for, if the catalog says.
    pub description: Option<String>,
    /// Its media type, if the catalog says.
    pub mime_type: Option<String>,
    /// Extra metadata the catalog attached, passed through untouched.
    pub meta: Option<Value>,
}

/// Whether, and in what shape, a registered app is live.
#[derive(Debug, Clone, PartialEq)]
pub struct ManagedRuntime {
    /// Whether models may use the app's tools now.
    pub enabled: bool,
    /// The tools the person has turned on, in catalog order.
    pub enabled_tools: Vec<String>,
    /// The widget the tools use, if any.
    pub widget: Option<WidgetResource>,
}

/// An app that is, or was, exposed to one conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exposure {
    /// The app.
    pub app_id: String,
    /// Whether the person pinned it, which keeps it from being evicted.
    pub pinned: bool,
    /// When it was last used, on the host's own scale; larger is more recent.
    pub last_used: u64,
}

/// What the host currently holds for a published app. Each part is `None` when
/// the host holds nothing of that kind; the parts are independent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Published {
    /// The name of the app's logical MCP server.
    pub server_name: Option<String>,
    /// Whether the app is live, if the host has runtime state for it.
    pub enabled: Option<bool>,
    /// The tools live now, if the host has runtime state that names them.
    pub enabled_tools: Option<Vec<String>>,
    /// The widget its tools use, if the host has runtime state that carries one.
    pub widget: Option<WidgetResource>,
}

/// Where the service publishes an app's MCP tools.
///
/// A host without a way to expose per-conversation tools provides none, and
/// publication quietly does nothing. Every method reports a failure as text:
/// the service shows it to the person or logs it, and does not branch on it.
#[async_trait]
pub trait McpPublisher: Send + Sync {
    /// Whether the host can publish right now. A publisher whose registry has
    /// gone away is not available, and the service treats it as absent.
    fn available(&self) -> bool;

    /// Record `app` and its runtime state. Repeating it with new state updates
    /// it; nothing is exposed to any conversation yet.
    async fn publish(&self, app: &ManagedApp, runtime: ManagedRuntime) -> Result<(), String>;

    /// Forget the app. Conversations stop being offered its tools.
    async fn unregister(&self, app_id: &str) -> Result<(), String>;

    /// Drop the app's live connection, if it has one, without forgetting it.
    async fn disconnect_app(&self, app_id: &str) -> Result<(), String>;

    /// Drop every managed app's live connection, without forgetting any.
    async fn disconnect_all(&self) -> Result<(), String>;

    /// Offer the published app's tools to one conversation, pinned when `pin`.
    /// If the host's budget of live apps is full it evicts another and closes
    /// that one's connection itself.
    async fn expose(
        &self,
        conversation_id: &str,
        app: &ManagedApp,
        pin: bool,
    ) -> Result<(), String>;

    /// Unpin an app that is exposed to a conversation. One that is not exposed
    /// is not an error.
    async fn unpin(&self, conversation_id: &str, app_id: &str) -> Result<(), String>;

    /// What the host holds for the app right now. The inventory shows this
    /// rather than what the service intended, because it is the truth about
    /// the process.
    async fn published(&self, app_id: &str) -> Published;

    /// Take a call lease on an app exposed to a conversation. The host rejects
    /// a call it has no room for rather than queueing it without bound, and
    /// every lease taken must be released with [`end_call`](Self::end_call).
    async fn begin_call(&self, conversation_id: &str, app_id: &str) -> Result<(), String>;

    /// Release a lease taken by [`begin_call`](Self::begin_call). Releasing one
    /// that is not held is harmless, so cleanup on a timeout or a cancellation
    /// cannot become a second failure.
    async fn end_call(&self, conversation_id: &str, app_id: &str);

    /// The apps exposed to a conversation, in no particular order.
    async fn exposures(&self, conversation_id: &str) -> Vec<Exposure>;
}
