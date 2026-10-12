//! Startup connects off the session-open path.
//!
//! Claude Code's interactive REPL opens before its MCP servers finish
//! connecting: each server starts `pending`, its tools join the tool list once
//! it connects, and `WaitForMcpServers` lets the model wait out a server it
//! needs. A host that wants the same behavior opens this window before its
//! startup [`McpRegistry::connect_all`] calls and closes it once the session is
//! assembled. Calls made after the window closes (reloads, deferred SDK
//! servers) dial in the foreground again.

use super::McpRegistry;
use crate::connection::{McpConnectionState, McpServerConfig};
use std::sync::{Arc, PoisonError};
use std::time::SystemTime;
use tokio::sync::watch;

/// The open flag and the dial tasks started while it was set.
#[derive(Default)]
pub(super) struct BackgroundConnects {
    open: bool,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// Releases one in-flight slot when its dial ends, including when shutdown
/// aborts the task, so watchers never wait on a dial that no longer exists.
struct InflightSlot(Arc<watch::Sender<usize>>);

impl Drop for InflightSlot {
    fn drop(&mut self) {
        self.0
            .send_modify(|inflight| *inflight = inflight.saturating_sub(1));
    }
}

impl McpRegistry {
    /// Open the background-connect window.
    ///
    /// Until [`Self::end_background_connects`], [`Self::connect_all`] publishes
    /// each enabled server as `Connecting` (so `/mcp`, host snapshots and
    /// `WaitForMcpServers` see it as pending) and dials it on its own task
    /// instead of holding the caller. Disabled servers are recorded exactly as
    /// in the foreground path.
    pub fn begin_background_connects(&self) {
        self.background_connects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .open = true;
    }

    /// Close the window. Dials already started keep running; watch
    /// [`Self::watch_background_connects`] for them to settle.
    pub fn end_background_connects(&self) {
        let mut window = self
            .background_connects
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        window.open = false;
        window.tasks.retain(|task| !task.is_finished());
    }

    /// The number of background dials still running. It changes once per dial
    /// start and once per settle (connected, cached, failed or needs-auth), so
    /// a host can re-publish its live catalog as each startup server settles.
    #[must_use]
    pub fn watch_background_connects(&self) -> watch::Receiver<usize> {
        self.background_inflight.subscribe()
    }

    /// Abort every background dial still running. Session shutdown calls this
    /// so an abandoned dial cannot outlive its runtime.
    pub fn abort_background_connects(&self) {
        let tasks = std::mem::take(
            &mut self
                .background_connects
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .tasks,
        );
        for task in tasks {
            task.abort();
        }
    }

    pub(super) fn background_connects_open(&self) -> bool {
        self.background_connects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .open
    }

    /// The window's [`Self::connect_all`]: record every server under its
    /// lifecycle lock, then dial each enabled one on a detached task.
    pub(super) async fn connect_all_in_background(&self, configs: Vec<McpServerConfig>) {
        self.freeze_configuration();
        for config in configs {
            let name = config.name.clone();
            {
                let lifecycle = self.lifecycle_lock(&name);
                let _guard = lifecycle.lock().await;
                let mut conns = self.connections.write().await;
                if config.disabled {
                    Self::record_disabled_server(&mut conns, config);
                    tracing::debug!(server = %name, "skipping disabled MCP server");
                    continue;
                }
                if matches!(
                    conns.get(&name),
                    Some(McpConnectionState::Connected { .. } | McpConnectionState::Cached { .. })
                ) {
                    continue;
                }
                conns.insert(
                    name,
                    McpConnectionState::Connecting {
                        config: config.clone(),
                        started_at: SystemTime::now(),
                    },
                );
            }
            self.background_inflight
                .send_modify(|inflight| *inflight += 1);
            let slot = InflightSlot(Arc::clone(&self.background_inflight));
            let registry = self.clone_for_background();
            let task = tokio::spawn(async move {
                let _slot = slot;
                registry.connect_published_server(config).await;
            });
            self.background_connects
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .tasks
                .push(task);
        }
    }

    /// Dial one server published by [`Self::connect_all_in_background`]. A
    /// lifecycle operation that ran first (disable, stop, a foreground
    /// connect) replaced the published `Connecting` state and wins.
    async fn connect_published_server(&self, config: McpServerConfig) {
        let name = config.name.clone();
        let lifecycle = self.lifecycle_lock(&name);
        let _guard = lifecycle.lock().await;
        if !matches!(
            self.connections.read().await.get(&name),
            Some(McpConnectionState::Connecting { .. })
        ) {
            return;
        }
        if let Err(error) = self.connect_locked(config, None).await {
            tracing::warn!(server = %name, %error, "MCP auto-connect failed");
        }
    }
}
