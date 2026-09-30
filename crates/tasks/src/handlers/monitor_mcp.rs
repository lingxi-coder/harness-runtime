//! `MonitorMcp` task handler — follows an MCP server's resource catalog and
//! emits a spool line on every detected change.
//!
//! # Relationship to claude-code's `monitor_mcp` (MON-12)
//!
//! Read this before treating anything here as parity work: the two are NOT
//! the same thing, and an earlier version of this header conflated them.
//!
//! 2.1.263 has a `monitor_mcp` TASK TYPE — prefix `m` in the id table
//! (`src_160932144.js` @1343), labelled `"monitor"`
//! (`src_160988549.js` @2022918), counted as live work by the monitor
//! predicates (`r2t`, `JLe`), and projected to hooks as
//! `{server, tool}` (`src_160988549.js` @5136526, the same pair `mcp_task`
//! carries). What it does NOT have is a producer: nothing in 2.1.263 creates
//! one, and there is no kill module for it. The type is dormant there.
//!
//! It is dormant HERE too — no call site spawns a `MonitorMcp` task; this
//! handler is registered and never invoked in production.
//!
//! The notification-driven refresh the old header cited is real but belongs to
//! MCP CONNECTION management, not to this task type: `onMcpResourceListChanged`
//! (`src_180597926.js` @1451792) re-fetches a server's resources, templates and
//! commands when it pushes `notifications/resources/list_changed`. It creates
//! no task and writes no spool.
//!
//! So the resource-catalog watcher below is a LingXi shape with no oracle twin.
//! It is kept — a dormant handler costs nothing and the wiring is the hard part
//! — but its behaviour is not evidence about claude-code, and the oracle's
//! `{server, tool}` projection is what a real producer would have to fill.
//!
//! # Why the design is push-with-poll-fallback
//!
//! The registry/client expose a per-connection notification stream. This
//! handler performs one initial reconciliation, then waits for
//! `notifications/resources/list_changed` before re-fetching. It falls back
//! to the existing interval only when the live transport cannot provide a
//! stream (or a connection closes), preserving visibility for transports that
//! have not implemented server push yet.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::StreamExt;
use lingxi_core::host::{BackgroundTaskHandle, FileSystem};
use tokio::sync::{watch, Mutex};

use crate::id::{generate_task_id, TaskType};
use crate::output_manager::TaskOutputManager;
use crate::task_trait::{Task, TaskContext, TaskError, TaskHandle, TaskSpawnInput};

/// Default poll cadence. There is nothing to be faithful to — 2.1.263 has no
/// producer for this task type at all (see the module header) — so any sane
/// default is parity-neutral; 5s keeps catalog drift visible without hammering
/// the server.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);

const RESOURCE_LIST_CHANGED_METHOD: &str = "notifications/resources/list_changed";

/// Whether a server notification invalidates the resource catalog.
///
/// MCP standardizes the `notifications/` prefix. The unprefixed spelling is
/// accepted as a compatibility measure for older platform adapters that
/// exposed the suffix directly; other notifications must not trigger a
/// potentially expensive catalog read.
fn is_resource_list_changed(method: &str) -> bool {
    matches!(
        method,
        RESOURCE_LIST_CHANGED_METHOD | "resources/list_changed"
    )
}

/// Per-task control block held by the handler so [`Task::kill`] (and the
/// `cleanup` closure on [`TaskHandle`]) can cooperatively stop the poll loop.
struct MonitorEntry {
    /// Retained cancellation state. Unlike `Notify`, a watch value cannot lose
    /// a stop signal sent just before the worker starts awaiting it.
    stop: watch::Sender<bool>,
    /// Background handle returned by the runtime spawner, for hard cancel.
    handle: BackgroundTaskHandle,
}

/// Handler for [`TaskType::MonitorMcp`].
///
/// Everything the poll loop needs beyond what [`TaskContext`] supplies
/// (`fs` + `runtime`) is captured at construction, because `TaskContext`
/// carries neither an [`mcp::McpRegistry`] nor a [`TaskOutputManager`].
pub struct MonitorMcpHandler {
    /// Registry used to resolve the live [`mcp::McpClient`] for a server.
    mcp: Arc<mcp::McpRegistry>,
    /// Spool-file owner; used to allocate the per-task output path.
    output: Arc<TaskOutputManager>,
    /// Running poll loops keyed by task id, so `kill` can stop them.
    entries: Arc<Mutex<HashMap<String, MonitorEntry>>>,
    /// Interval between catalog polls.
    poll_interval: Duration,
}

impl MonitorMcpHandler {
    /// Construct a handler.
    ///
    /// `fs` and `runtime` are intentionally **not** injected here — they
    /// arrive per call via [`TaskContext`] (the loop is spawned through
    /// `ctx.runtime` and writes through `ctx.fs`, never `tokio::spawn`; D17).
    #[must_use]
    pub fn new(
        mcp: Arc<mcp::McpRegistry>,
        output: Arc<TaskOutputManager>,
        poll_interval: Duration,
    ) -> Self {
        Self {
            mcp,
            output,
            entries: Arc::new(Mutex::new(HashMap::new())),
            poll_interval,
        }
    }

    /// Convenience constructor using [`DEFAULT_POLL_INTERVAL`].
    #[must_use]
    pub fn with_default_interval(
        mcp: Arc<mcp::McpRegistry>,
        output: Arc<TaskOutputManager>,
    ) -> Self {
        Self::new(mcp, output, DEFAULT_POLL_INTERVAL)
    }
}

/// Wall-clock seconds since the Unix epoch, for spool-line timestamps. Falls
/// back to `0` if the clock is before the epoch (never in practice).
fn unix_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Compute a per-resource fingerprint from the catalog entry alone (uri +
/// mime). Used when no per-URI content read is requested.
fn list_signature(mime_type: &Option<String>) -> String {
    format!("mime:{}", mime_type.as_deref().unwrap_or(""))
}

/// One poll tick. Resolves the client, lists (and optionally reads) the
/// watched resources, diffs against `prev`, and appends a spool line per
/// change. Returns the new snapshot to carry into the next tick. `prev` is
/// `None` on the very first tick so the initial catalog is recorded silently
/// (we only emit *changes*, matching claude-code which reacts to deltas).
async fn poll_once(
    mcp: &mcp::McpRegistry,
    fs: &Arc<dyn FileSystem>,
    spool: &str,
    server_name: &str,
    watch: &[String],
    prev: Option<&HashMap<String, String>>,
) -> HashMap<String, String> {
    // 1. Resolve the live client. A disconnected server is not fatal: the
    //    registry's reconnect loop may bring it up on a later tick.
    let Some(client) = mcp.get_client(server_name).await else {
        if prev.is_none() {
            // Only note the gap once per uninterrupted disconnected streak by
            // emitting on the first tick; reuse the previous (empty) snapshot.
            append_line(
                fs,
                spool,
                &format!("[{}] mcp:{server_name} server not connected", unix_ts()),
            )
            .await;
        }
        return prev.cloned().unwrap_or_default();
    };

    // 2. List the catalog; on RPC failure, surface it and keep the old snapshot.
    let resources = match client.list_resources().await {
        Ok(r) => r,
        Err(e) => {
            append_line(
                fs,
                spool,
                &format!(
                    "[{}] mcp:{server_name} list_resources failed: {e}",
                    unix_ts()
                ),
            )
            .await;
            return prev.cloned().unwrap_or_default();
        }
    };

    // 3. Build this tick's snapshot. If `watch` URIs are given, restrict to
    //    them and fingerprint by content (read_resource); otherwise watch the
    //    whole catalog and fingerprint by list signature (uri + mime).
    let mut snapshot: HashMap<String, String> = HashMap::new();
    if watch.is_empty() {
        for r in &resources {
            snapshot.insert(r.uri.clone(), list_signature(&r.mime_type));
        }
    } else {
        let present: HashMap<&str, &Option<String>> = resources
            .iter()
            .map(|r| (r.uri.as_str(), &r.mime_type))
            .collect();
        for uri in watch {
            // Only fingerprint watched URIs the server currently advertises.
            if let Some(mime) = present.get(uri.as_str()) {
                // Prefer a content hash so we catch *content* changes, not
                // just catalog membership. Fall back to the list signature if
                // the read fails.
                let sig = match client.read_resource(uri).await {
                    Ok(c) => {
                        let mut hasher = DefaultHasher::new();
                        c.content.hash(&mut hasher);
                        format!("len:{};hash:{:016x}", c.content.len(), hasher.finish())
                    }
                    Err(_) => list_signature(mime),
                };
                snapshot.insert(uri.clone(), sig);
            }
        }
    }

    // 4. Diff against the previous tick. Silent on the first tick.
    if let Some(prev) = prev {
        let ts = unix_ts();
        for (uri, sig) in &snapshot {
            match prev.get(uri) {
                None => {
                    append_line(
                        fs,
                        spool,
                        &format!("[{ts}] mcp:{server_name} resource added: {uri}"),
                    )
                    .await;
                }
                Some(old) if old != sig => {
                    append_line(
                        fs,
                        spool,
                        &format!("[{ts}] mcp:{server_name} resource changed: {uri}"),
                    )
                    .await;
                }
                Some(_) => {}
            }
        }
        for uri in prev.keys() {
            if !snapshot.contains_key(uri) {
                append_line(
                    fs,
                    spool,
                    &format!("[{ts}] mcp:{server_name} resource removed: {uri}"),
                )
                .await;
            }
        }
    }

    snapshot
}

/// Append `line` (a `\n` is added) to the spool, swallowing I/O errors — a
/// monitor must not crash the loop because one append failed.
async fn append_line(fs: &Arc<dyn FileSystem>, spool: &str, line: &str) {
    if let Err(e) = fs.append_file(spool, &format!("{line}\n")).await {
        tracing::warn!(target: "lingxi_tasks::monitor_mcp", spool, error = %e, "spool append failed");
    }
}

#[async_trait]
impl Task for MonitorMcpHandler {
    fn name(&self) -> &str {
        "monitor_mcp"
    }

    fn task_type(&self) -> TaskType {
        TaskType::MonitorMcp
    }

    async fn spawn(
        &self,
        input: TaskSpawnInput,
        ctx: TaskContext,
    ) -> Result<TaskHandle, TaskError> {
        let TaskSpawnInput::MonitorMcp { server_name, watch } = input else {
            return Err(TaskError::UnknownType);
        };

        // Allocate the task id + spool file. (When dispatched from
        // `TaskRegistry::create` the state already carries an `output_file`;
        // when the handler is driven directly we allocate our own. Either way
        // `allocate` is idempotent enough — it (re)creates an empty file.)
        let task_id = generate_task_id(TaskType::MonitorMcp);
        let spool_path = self
            .output
            .allocate(&task_id)
            .await
            .map_err(|e| TaskError::Io(e.to_string()))?;
        let spool = spool_path
            .to_str()
            .ok_or_else(|| TaskError::Internal("non-utf8 spool path".into()))?
            .to_string();

        // Clone everything the detached loop captures by value.
        let mcp = self.mcp.clone();
        let fs = ctx.fs.clone();
        let runtime = ctx.runtime.clone();
        let interval = self.poll_interval;
        let (stop, mut stop_loop) = watch::channel(false);
        let server_for_loop = server_name.clone();
        let watch_for_loop = watch.clone();

        // Subscribe before the first reconciliation. If the server mutates its
        // catalog while the snapshot is in flight, the queued notification is
        // consumed afterwards; snapshot-before-subscribe would lose that edge
        // and could leave a push-only monitor stale forever.
        let fut = Box::pin(async move {
            let mut notifications = tokio::select! {
                _ = stop_loop.changed() => return,
                stream = mcp.subscribe_notifications(&server_for_loop) => stream.ok(),
            };
            if *stop_loop.borrow() {
                return;
            }
            let first = tokio::select! {
                _ = stop_loop.changed() => return,
                snapshot = poll_once(
                    &mcp,
                    &fs,
                    &spool,
                    &server_for_loop,
                    &watch_for_loop,
                    None,
                ) => snapshot,
            };
            let mut prev = Some(first);

            'monitor: loop {
                if *stop_loop.borrow() {
                    break;
                }

                if let Some(stream) = notifications.as_mut() {
                    // Waiting on the stop notifier is essential here: an
                    // idle MCP server must still be cancellable without
                    // waiting for another notification or poll interval.
                    let stream_ended = tokio::select! {
                        _ = stop_loop.changed() => break,
                        notification = stream.next() => match notification {
                            Some(notification) if is_resource_list_changed(&notification.method) => {
                                let snapshot = tokio::select! {
                                    _ = stop_loop.changed() => break 'monitor,
                                    snapshot = poll_once(
                                        &mcp,
                                        &fs,
                                        &spool,
                                        &server_for_loop,
                                        &watch_for_loop,
                                        prev.as_ref(),
                                    ) => snapshot,
                                };
                                prev = Some(snapshot);
                                false
                            }
                            Some(_) => false,
                            None => true,
                        },
                    };
                    if stream_ended {
                        // A closed stream means this connection can no longer
                        // deliver notifications. Fall back and retry after
                        // the normal cadence so a reconnect can restore push.
                        notifications = None;
                    }
                } else {
                    tokio::select! {
                        _ = stop_loop.changed() => break,
                        _ = runtime.sleep(interval) => {}
                    }
                    if *stop_loop.borrow() {
                        break;
                    }
                    // Re-establish push before reconciling so the retry path
                    // has the same no-gap ordering as startup.
                    notifications = tokio::select! {
                        _ = stop_loop.changed() => break,
                        stream = mcp.subscribe_notifications(&server_for_loop) => stream.ok(),
                    };
                    if *stop_loop.borrow() {
                        break;
                    }
                    let snapshot = tokio::select! {
                        _ = stop_loop.changed() => break,
                        snapshot = poll_once(
                            &mcp,
                            &fs,
                            &spool,
                            &server_for_loop,
                            &watch_for_loop,
                            prev.as_ref(),
                        ) => snapshot,
                    };
                    prev = Some(snapshot);
                }
            }
        });

        // Spawn through the runtime spawner — never `tokio::spawn` (D17).
        let handle = ctx
            .runtime
            .spawn(&format!("monitor_mcp:{server_name}"), fut)
            .await
            .map_err(|e| TaskError::Internal(e.to_string()))?;

        // Record the control block so `kill` can stop the loop.
        self.entries.lock().await.insert(
            task_id.clone(),
            MonitorEntry {
                stop: stop.clone(),
                handle,
            },
        );

        // `cleanup` flips the cooperative stop flag (it cannot `.await` the
        // runtime cancel, so the registry / `kill` performs the hard cancel).
        let cleanup_stop = stop;
        let cleanup: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = cleanup_stop.send(true);
        });

        Ok(TaskHandle::new(task_id, Some(cleanup)))
    }

    async fn kill(&self, task_id: &str, ctx: TaskContext) -> Result<(), TaskError> {
        let entry = self.entries.lock().await.remove(task_id);
        let Some(entry) = entry else {
            return Err(TaskError::NotFound(task_id.to_string()));
        };
        // Cooperative stop first, then hard cancel through the runtime.
        let _ = entry.stop.send(true);
        ctx.runtime
            .cancel(&entry.handle)
            .await
            .map_err(|e| TaskError::Internal(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::map_unwrap_or,
    clippy::unwrap_used
)]
mod tests {
    use super::*;
    use std::collections::HashMap as StdHashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use tokio::sync::{Mutex as TokioMutex, Notify};

    use lingxi_core::host::{
        ElicitRequestDto, ElicitResultDto, FileContent, FileEvent, FlockGuard, FsError, McpError,
        McpNotificationStream, McpPromptDto, McpRawConnection, McpResourceContentDto,
        McpResourceDto, McpToolDto, McpToolResultDto, McpTransport, McpTransportKind,
        McpTransportSpec, RuntimeError, RuntimeSpawner, ServerCapabilitiesDto,
    };
    use lingxi_core::types::McpConnectionId;
    use test_harness::mocks::{MockMcpTransport, MockRuntimeSpawner};

    // ---- minimal in-memory FileSystem (mirrors handle.rs tests) ----------
    struct InMemoryFs {
        files: TokioMutex<StdHashMap<String, String>>,
    }
    impl InMemoryFs {
        fn new() -> Self {
            Self {
                files: TokioMutex::new(StdHashMap::new()),
            }
        }
    }
    #[async_trait]
    impl FileSystem for InMemoryFs {
        async fn read_file(
            &self,
            path: &str,
            offset: Option<u64>,
            limit: Option<u64>,
        ) -> Result<FileContent, FsError> {
            let map = self.files.lock().await;
            let content = map.get(path).cloned().unwrap_or_default();
            let off = offset.unwrap_or(0) as usize;
            let body: String = content.chars().skip(off).collect();
            let truncated = limit.is_some_and(|lim| body.len() as u64 > lim);
            let trimmed = match limit {
                Some(lim) => body.chars().take(lim as usize).collect(),
                None => body,
            };
            let total_lines = content.lines().count() as u64;
            Ok(FileContent {
                content: trimmed,
                truncated,
                total_lines,
            })
        }
        async fn write_file(&self, path: &str, body: &str) -> Result<(), FsError> {
            self.files
                .lock()
                .await
                .insert(path.to_string(), body.to_string());
            Ok(())
        }
        fn is_within_workspace(&self, _: &str) -> bool {
            true
        }
        async fn watch(
            &self,
            _: &str,
        ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = FileEvent> + Send>>, FsError>
        {
            Err(FsError::Io("not supported".into()))
        }
        async fn append_file(&self, path: &str, body: &str) -> Result<(), FsError> {
            let mut map = self.files.lock().await;
            map.entry(path.to_string()).or_default().push_str(body);
            Ok(())
        }
        async fn truncate(&self, path: &str, len: u64) -> Result<(), FsError> {
            let mut map = self.files.lock().await;
            if let Some(s) = map.get_mut(path) {
                s.truncate(len as usize);
            }
            Ok(())
        }
        async fn file_mtime(&self, _: &str) -> Result<std::time::SystemTime, FsError> {
            Ok(std::time::SystemTime::UNIX_EPOCH)
        }
        async fn file_size(&self, path: &str) -> Result<u64, FsError> {
            Ok(self
                .files
                .lock()
                .await
                .get(path)
                .map(|s| s.len() as u64)
                .unwrap_or(0))
        }
        async fn delete_file(&self, path: &str) -> Result<(), FsError> {
            self.files.lock().await.remove(path);
            Ok(())
        }
        async fn symlink(&self, _: &str, _: &str) -> Result<(), FsError> {
            Ok(())
        }
        async fn flock_exclusive(&self, _: &str) -> Result<Box<dyn FlockGuard>, FsError> {
            Err(FsError::Io("not supported".into()))
        }
        async fn fsync(&self, _: &str) -> Result<(), FsError> {
            Ok(())
        }
    }

    fn make_handler() -> (
        tempfile::TempDir,
        Arc<dyn FileSystem>,
        Arc<MockRuntimeSpawner>,
        MonitorMcpHandler,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let runtime = Arc::new(MockRuntimeSpawner::default());
        let out = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));
        let mcp = Arc::new(mcp::McpRegistry::new(Arc::new(MockMcpTransport::new())));
        // Fast poll so the test loop ticks several times quickly.
        let handler = MonitorMcpHandler::new(mcp, out, Duration::from_millis(5));
        (dir, fs, runtime, handler)
    }

    fn ctx(fs: Arc<dyn FileSystem>, runtime: Arc<MockRuntimeSpawner>) -> TaskContext {
        TaskContext {
            fs,
            runtime: runtime as Arc<dyn RuntimeSpawner>,
        }
    }

    /// Transport with a live, never-ending notification stream. The
    /// subscription marker lets the test distinguish a monitor waiting on
    /// push from one still in its polling fallback.
    struct PendingNotificationTransport {
        subscribed: Arc<Notify>,
    }

    impl PendingNotificationTransport {
        fn new() -> (Arc<Self>, Arc<Notify>) {
            let subscribed = Arc::new(Notify::new());
            (
                Arc::new(Self {
                    subscribed: subscribed.clone(),
                }),
                subscribed,
            )
        }
    }

    #[async_trait]
    impl McpTransport for PendingNotificationTransport {
        async fn connect(&self, _spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
            Err(McpError::Internal("test transport does not connect".into()))
        }

        async fn initialize(
            &self,
            _conn: &McpRawConnection,
        ) -> Result<ServerCapabilitiesDto, McpError> {
            Ok(ServerCapabilitiesDto {
                tools: false,
                resources: false,
                prompts: false,
                logging: false,
                directory_read: false,
                experimental: StdHashMap::new(),
                extensions: StdHashMap::new(),
            })
        }

        async fn list_tools(&self, _conn: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
            Ok(Vec::new())
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
            _tool: &str,
            _input: serde_json::Value,
        ) -> Result<McpToolResultDto, McpError> {
            Ok(McpToolResultDto::default())
        }

        async fn read_resource(
            &self,
            _conn: &McpRawConnection,
            _uri: &str,
        ) -> Result<McpResourceContentDto, McpError> {
            Err(McpError::Internal("test transport does not read".into()))
        }

        async fn ping(&self, _conn_id: McpConnectionId) -> Result<(), McpError> {
            Ok(())
        }

        async fn notifications(
            &self,
            _conn: &McpRawConnection,
        ) -> Result<McpNotificationStream, McpError> {
            self.subscribed.notify_one();
            Ok(Box::pin(futures::stream::pending()))
        }

        async fn handle_elicitation(
            &self,
            _conn: &McpRawConnection,
            _request: ElicitRequestDto,
        ) -> Result<ElicitResultDto, McpError> {
            Err(McpError::Internal("test transport does not elicit".into()))
        }

        async fn disconnect(&self, _conn_id: McpConnectionId) -> Result<(), McpError> {
            Ok(())
        }

        fn supported_transports(&self) -> Vec<McpTransportKind> {
            vec![McpTransportKind::InProcess]
        }
    }

    /// Runtime that reports when a spawned task returns, allowing the
    /// cancellation test to verify the notification wait is woken by cleanup.
    struct CompletionRuntime {
        next_id: AtomicU64,
        sleep_calls: AtomicUsize,
        finished: Arc<Notify>,
    }

    impl CompletionRuntime {
        fn new() -> (Arc<Self>, Arc<Notify>) {
            let finished = Arc::new(Notify::new());
            (
                Arc::new(Self {
                    next_id: AtomicU64::new(1),
                    sleep_calls: AtomicUsize::new(0),
                    finished: finished.clone(),
                }),
                finished,
            )
        }
    }

    #[async_trait]
    impl RuntimeSpawner for CompletionRuntime {
        async fn spawn(
            &self,
            name: &str,
            task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) -> Result<lingxi_core::host::BackgroundTaskHandle, RuntimeError> {
            let finished = self.finished.clone();
            tokio::spawn(async move {
                task.await;
                finished.notify_one();
            });
            Ok(lingxi_core::host::BackgroundTaskHandle {
                task_name: name.to_string(),
                task_id: self.next_id.fetch_add(1, Ordering::SeqCst),
            })
        }

        async fn sleep(&self, duration: Duration) {
            self.sleep_calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(duration).await;
        }

        async fn cancel(
            &self,
            _handle: &lingxi_core::host::BackgroundTaskHandle,
        ) -> Result<(), RuntimeError> {
            Ok(())
        }
    }

    async fn connected_notification_registry(
        transport: Arc<PendingNotificationTransport>,
    ) -> Arc<mcp::McpRegistry> {
        let registry = Arc::new(mcp::McpRegistry::new(transport));
        let config = mcp::McpServerConfig {
            name: "mock".into(),
            spec: McpTransportSpec::InProcess {
                registry_key: "mock".into(),
            },
            scope: mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
            disabled: false,
            timeout_ms: None,
            discovery_cache: None,
            always_load: false,
            tools: Vec::new(),
            tool_permissions: std::collections::BTreeMap::new(),
            config_error: None,
            metadata: mcp::McpServerMetadata::default(),
        };
        registry.connections.write().await.insert(
            "mock".into(),
            mcp::McpConnectionState::Connected {
                config,
                connection_id: McpConnectionId::new(),
                capabilities: ServerCapabilitiesDto {
                    tools: false,
                    resources: false,
                    prompts: false,
                    logging: false,
                    directory_read: false,
                    experimental: StdHashMap::new(),
                    extensions: StdHashMap::new(),
                },
                negotiated: lingxi_core::host::McpNegotiatedProtocol {
                    era: lingxi_core::host::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: Vec::new(),
                resources: Vec::new(),
                resource_templates: Vec::new(),
                prompts: Vec::new(),
                connected_at: SystemTime::now(),
            },
        );
        registry
    }

    /// Non-`MonitorMcp` input is rejected with `UnknownType`.
    #[tokio::test]
    async fn spawn_rejects_wrong_input_variant() {
        let (_d, fs, runtime, handler) = make_handler();
        let res = handler
            .spawn(
                TaskSpawnInput::LocalBash {
                    command: "echo".into(),
                    timeout: None,
                    tool_use_id: None,
                },
                ctx(fs, runtime),
            )
            .await;
        assert!(
            matches!(res, Err(TaskError::UnknownType)),
            "wrong input variant must be rejected"
        );
    }

    /// `poll_once` against a registry with NO client emits "not connected"
    /// on the first tick and returns an empty snapshot. This directly
    /// exercises the diff/emit path without needing a live `McpClient`
    /// (whose only constructor requires a real `jsonrpc::Connection`).
    #[tokio::test]
    async fn poll_emits_not_connected_for_unknown_server() {
        let (_d, fs, _rt, handler) = make_handler();
        let spool = "/tmp/monitor-test-spool.txt";
        fs.write_file(spool, "").await.unwrap();

        let snap = poll_once(&handler.mcp, &fs, spool, "ghost", &[], None).await;
        assert!(snap.is_empty(), "no resources from a disconnected server");

        let body = fs.read_file(spool, None, None).await.unwrap().content;
        assert!(
            body.contains("mcp:ghost server not connected"),
            "first tick records the disconnect, got: {body:?}"
        );
    }

    #[test]
    fn only_resource_catalog_notifications_trigger_reconciliation() {
        assert!(is_resource_list_changed(
            "notifications/resources/list_changed"
        ));
        assert!(is_resource_list_changed("resources/list_changed"));
        assert!(!is_resource_list_changed(
            "notifications/tools/list_changed"
        ));
        assert!(!is_resource_list_changed("notifications/progress"));
    }

    #[tokio::test]
    async fn cleanup_wakes_push_subscription_without_polling() {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let output = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));
        let (transport, subscribed) = PendingNotificationTransport::new();
        let registry = connected_notification_registry(transport).await;
        let handler = MonitorMcpHandler::new(registry, output, Duration::from_secs(5));
        let (runtime, finished) = CompletionRuntime::new();
        let ctx = TaskContext {
            fs,
            runtime: runtime.clone() as Arc<dyn RuntimeSpawner>,
        };

        let handle = handler
            .spawn(
                TaskSpawnInput::MonitorMcp {
                    server_name: "mock".into(),
                    watch: Vec::new(),
                },
                ctx,
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), subscribed.notified())
            .await
            .expect("monitor must subscribe to the live notification stream");
        tokio::task::yield_now().await;
        assert_eq!(
            runtime.sleep_calls.load(Ordering::SeqCst),
            0,
            "a live notification stream must not enter the polling fallback"
        );

        (handle.cleanup.as_ref().unwrap())();
        tokio::time::timeout(Duration::from_secs(2), finished.notified())
            .await
            .expect("cleanup must wake an idle push subscription");
    }

    /// Diff logic: a resource present in the new snapshot but absent in the
    /// previous one is reported as `added`; one whose signature changed is
    /// reported as `changed`; one that disappeared is `removed`. Verified
    /// against the registry's `get_client` returning `None` is not enough to
    /// reach the diff, so we drive the snapshot diff directly through a tiny
    /// helper-shaped scenario using `poll_once`'s `prev` argument semantics.
    #[tokio::test]
    async fn diff_reports_add_change_remove() {
        // Build prev/new snapshots and feed them through the same string
        // formatting the loop uses, by writing expected lines and asserting
        // the format is stable. We exercise the real append + format path.
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spool = "/tmp/monitor-diff-spool.txt";
        fs.write_file(spool, "").await.unwrap();

        // Simulate one diff tick by hand using the same emit helpers.
        let mut prev: StdHashMap<String, String> = StdHashMap::new();
        prev.insert("a".into(), "mime:text".into());
        prev.insert("b".into(), "mime:text".into()); // will be removed
        let mut new: StdHashMap<String, String> = StdHashMap::new();
        new.insert("a".into(), "mime:json".into()); // changed
        new.insert("c".into(), "mime:text".into()); // added

        let ts = unix_ts();
        for (uri, sig) in &new {
            match prev.get(uri) {
                None => {
                    append_line(&fs, spool, &format!("[{ts}] mcp:s resource added: {uri}")).await
                }
                Some(old) if old != sig => {
                    append_line(&fs, spool, &format!("[{ts}] mcp:s resource changed: {uri}")).await;
                }
                Some(_) => {}
            }
        }
        for uri in prev.keys() {
            if !new.contains_key(uri) {
                append_line(&fs, spool, &format!("[{ts}] mcp:s resource removed: {uri}")).await;
            }
        }

        let body = fs.read_file(spool, None, None).await.unwrap().content;
        assert!(body.contains("resource added: c"), "got: {body:?}");
        assert!(body.contains("resource changed: a"), "got: {body:?}");
        assert!(body.contains("resource removed: b"), "got: {body:?}");
    }

    /// `spawn` returns a handle (with a `cleanup` hook) and registers the
    /// poll loop; `kill` stops it and removes the entry. With the mock
    /// transport's empty resource list the loop simply ticks; we assert the
    /// lifecycle (spawn registers, kill deregisters + cancels cleanly).
    #[tokio::test]
    async fn spawn_then_kill_lifecycle() {
        let (_d, fs, runtime, handler) = make_handler();
        let c = ctx(fs.clone(), runtime.clone());

        let h = handler
            .spawn(
                TaskSpawnInput::MonitorMcp {
                    server_name: "mock".into(),
                    watch: vec![],
                },
                c.clone(),
            )
            .await
            .unwrap();
        assert!(h.task_id.starts_with('m'), "monitor_mcp ids prefix 'm'");
        assert!(h.cleanup.is_some(), "cleanup hook stops the loop");
        assert_eq!(
            handler.entries.lock().await.len(),
            1,
            "spawn registers the loop"
        );

        // Let the loop tick a couple times.
        runtime.sleep(Duration::from_millis(20)).await;

        handler.kill(&h.task_id, c).await.unwrap();
        assert!(
            handler.entries.lock().await.is_empty(),
            "kill deregisters the loop"
        );

        // Killing an unknown id is NotFound.
        let err = handler.kill("nope", ctx(fs, runtime)).await.unwrap_err();
        assert!(matches!(err, TaskError::NotFound(_)));
    }

    /// The `cleanup` closure on the returned handle flips the cooperative
    /// stop flag so the loop exits at its next tick.
    #[tokio::test]
    async fn cleanup_hook_signals_stop() {
        let (_d, fs, runtime, handler) = make_handler();
        let c = ctx(fs, runtime);
        let h = handler
            .spawn(
                TaskSpawnInput::MonitorMcp {
                    server_name: "mock".into(),
                    watch: vec!["res://x".into()],
                },
                c,
            )
            .await
            .unwrap();
        // Invoking cleanup must not panic and signals the loop to stop.
        (h.cleanup.as_ref().unwrap())();
    }
}
