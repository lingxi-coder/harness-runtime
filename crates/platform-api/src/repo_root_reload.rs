//! Composition-root seam for refreshing catalogs after a repository root is
//! registered at runtime.

use async_trait::async_trait;
use std::path::PathBuf;

/// Catalog refresh requested after a root has already been admitted by the
/// session sandbox and announced to MCP clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRootReloadRequest {
    /// Canonical root that was added to the live session.
    pub root: PathBuf,
    /// Re-scan skill directories and reconcile the shared command registry.
    pub reload_skills: bool,
    /// Re-read plugin settings and reconcile live plugin components.
    pub reload_plugins: bool,
}

/// Truthful, per-catalog result returned to protocol callers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoRootReloadOutcome {
    /// The skill catalog completed a live reconciliation.
    pub skills_reloaded: bool,
    /// The plugin catalog completed a live reconciliation.
    pub plugins_reloaded: bool,
    /// Non-fatal catalog failures. Root registration itself remains committed.
    pub errors: Vec<String>,
}

/// Host-owned catalog reconciler.
///
/// The orchestrator owns the security ordering, but only the desktop
/// composition root owns the live skill registry and plugin runtime.
#[async_trait]
pub trait RepoRootReloader: Send + Sync {
    /// Refresh the requested catalogs after `request.root` is trusted.
    async fn reload(&self, request: RepoRootReloadRequest) -> RepoRootReloadOutcome;
}
