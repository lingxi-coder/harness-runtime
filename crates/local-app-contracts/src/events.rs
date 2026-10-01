//! The managed MCP inventory a host shows, and the failures the service
//! surfaces to the person using an app.
//!
//! The events that carry these, and the sink that delivers them, are the
//! service's own (`local_app_service::host`); the payloads that ask a person
//! for something live in [`crate::approvals`], and the page bridge's answers in
//! [`crate::bridge`].
#![allow(missing_docs)] // the variants and fields are self-describing; the types carry the docs

use crate::approvals::{McpToolSurface, VerificationSummary};
use serde::{Deserialize, Serialize};

/// Derived publication state of an app. On the wire a bare string:
/// `"draft"`, `"published_unverified"` or `"published_verified"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationState {
    /// No active build/catalog pair is published.
    Draft,
    /// The app has an active build/catalog pair but no UI verification
    /// evidence.
    PublishedUnverified,
    /// The app has an active build/catalog pair and UI verification evidence.
    PublishedVerified,
}

/// Host-managed widget resource surfaced alongside one Local App MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpAppWidget {
    pub resource_uri: String,
    pub mime_type: String,
    pub resource_sha256: String,
}

/// Managed Local App MCP lifecycle state rendered in native inventory UIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedMcpStatus {
    Disabled,
    NeedsSetup,
    Authoring,
    Enabled,
    NeedsRevalidation,
    Error,
}

/// One managed Local App MCP logical-server row for native inventory UIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedMcpServer {
    pub server_name: String,
    pub app_id: String,
    pub app_name: String,
    #[serde(default)]
    pub enabled: bool,
    pub status: ManagedMcpStatus,
    pub settings_revision: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_tools: Vec<String>,
    /// Whether this app is pinned in the currently active conversation.
    #[serde(default)]
    pub pinned_to_current_conversation: bool,
    pub build_id: String,
    pub catalog_sha256: String,
    pub tool_surface_sha256: String,
    pub tool_count: u32,
    pub authoring_revision: u64,
    pub publication_state: PublicationState,
    pub mcp_verification: VerificationSummary,
    pub ui_verification: VerificationSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widget: Option<McpAppWidget>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<McpToolSurface>,
}

/// Why a Local App operation failed, as the host shows it to the person using
/// the app. These come from the plugin and control plane around an app (the
/// bundle, a catalog, a proposal), not from running the app itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginErrorCode {
    PluginDisabled,
    BuiltinBundleUnavailable,
    TemplateUnavailable,
    ProposalInvalid,
    CatalogStale,
    ActiveStateCorrupt,
    RevisionConflict,
    InvalidMcpSettings,
    McpAuthoringRequired,
    RepairBudgetExhausted,
    ExposureCapacityReached,
}
