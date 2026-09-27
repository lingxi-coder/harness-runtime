//! `LspDiagnosticRegistry` — accumulates `textDocument/publishDiagnostics`
//! notifications keyed by document URI and tracks document freshness.

use crate::diagnostics_format::{dedup_key, format_diagnostics_block, DiagnosticFile};
use lsp_types::{Diagnostic, Url};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, Notify, RwLock};

/// One snapshot for one document.
#[derive(Debug, Clone)]
pub struct DiagnosticEntry {
    /// Optional version (LSP `PublishDiagnosticsParams.version`). `None`
    /// when the server does not track versions.
    pub version: Option<i32>,
    /// Diagnostics list, in server-provided order.
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone)]
struct DiagnosticSnapshot {
    entry: DiagnosticEntry,
}

#[derive(Debug, Clone)]
struct DocumentState {
    host_path: PathBuf,
    server_path: PathBuf,
    last_synced_version: Option<i32>,
    sync_generation: u64,
    publish_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticFreshness {
    Fresh,
    AdvisoryNoVersion,
    AdvisoryVersionMismatch { expected: i32, actual: i32 },
}

#[derive(Debug, Clone)]
pub struct HostDiagnosticSnapshot {
    pub host_path: PathBuf,
    pub server_path: PathBuf,
    pub uri: Url,
    pub synced_version: Option<i32>,
    pub diagnostics_version: Option<i32>,
    pub freshness: DiagnosticFreshness,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSettleState {
    NoTrackedDocuments,
    Settled,
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagnosticSettleStatus {
    pub state: DiagnosticSettleState,
    pub tracked_documents: usize,
}

/// Independent consumer cursor for passive `<new-diagnostics>` reads.
#[derive(Debug, Default, Clone)]
pub struct DiagnosticConsumerCursor {
    seen: HashMap<Url, HashSet<String>>,
}

/// Independent diagnostics source with its own dedup cursor and optional host-root filter.
pub struct LspDiagnosticsSource {
    registry: LspDiagnosticRegistry,
    cursor: Mutex<DiagnosticConsumerCursor>,
    host_root: Option<PathBuf>,
    settle_timeout: Option<Duration>,
}

/// Thread-safe map from file URI to latest diagnostic snapshot plus document
/// sync metadata.
#[derive(Debug, Default, Clone)]
pub struct LspDiagnosticRegistry {
    inner: Arc<RwLock<HashMap<Url, DiagnosticSnapshot>>>,
    /// Global compatibility cursor used by `take_new_diagnostics_block`.
    sent: Arc<RwLock<HashMap<Url, HashSet<String>>>>,
    documents: Arc<RwLock<HashMap<Url, DocumentState>>>,
    next_publish_sequence: Arc<AtomicU64>,
    next_sync_generation: Arc<AtomicU64>,
    notify: Arc<Notify>,
}

impl LspDiagnosticRegistry {
    /// Empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the diagnostics for `uri` with `entry`.
    ///
    /// If `entry.version` is `Some` and an existing entry has a strictly
    /// greater version, the update is dropped (out-of-order delivery).
    pub async fn publish(&self, uri: Url, entry: DiagnosticEntry) {
        let _sequence = self.next_publish_sequence.fetch_add(1, Ordering::AcqRel) + 1;
        let mut guard = self.inner.write().await;
        if let (Some(new_v), Some(existing)) = (entry.version, guard.get(&uri)) {
            if let Some(existing_v) = existing.entry.version {
                if existing_v > new_v {
                    return;
                }
            }
        }
        guard.insert(uri.clone(), DiagnosticSnapshot { entry });
        drop(guard);
        if let Some(document) = self.documents.write().await.get_mut(&uri) {
            document.publish_generation = document.sync_generation;
        }
        self.notify.notify_waiters();
    }

    /// Record the host/server identity and last synced version for one document.
    pub async fn record_document_sync(
        &self,
        host_path: &Path,
        server_path: &Path,
        uri: Url,
        synced_version: Option<i32>,
        expect_publish: bool,
    ) {
        let existing_snapshot = self.inner.read().await.get(&uri).cloned();
        let sync_generation = if expect_publish {
            self.next_sync_generation.fetch_add(1, Ordering::AcqRel) + 1
        } else {
            0
        };
        let mut documents = self.documents.write().await;
        let entry = documents.entry(uri).or_insert_with(|| DocumentState {
            host_path: host_path.to_path_buf(),
            server_path: server_path.to_path_buf(),
            last_synced_version: synced_version,
            sync_generation,
            publish_generation: 0,
        });
        entry.host_path = host_path.to_path_buf();
        entry.server_path = server_path.to_path_buf();
        entry.last_synced_version = synced_version;
        if expect_publish {
            entry.sync_generation = sync_generation;
            entry.publish_generation = if existing_snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.entry.version == synced_version)
            {
                sync_generation
            } else {
                0
            };
        }
    }

    /// Snapshot the diagnostics for `uri` (empty if none).
    pub async fn get(&self, uri: &Url) -> Vec<Diagnostic> {
        self.inner
            .read()
            .await
            .get(uri)
            .map(|e| e.entry.diagnostics.clone())
            .unwrap_or_default()
    }

    /// Clear any diagnostics for `uri`. Idempotent.
    pub async fn clear(&self, uri: &Url) {
        self.inner.write().await.remove(uri);
    }

    /// Remove every document and diagnostic snapshot owned by `host_root`.
    ///
    /// Workspace LSP processes are isolated, so retaining their last
    /// diagnostics after the final workspace session closes would allow a
    /// later app session to observe stale findings through a fresh consumer
    /// cursor.
    pub async fn clear_under_host_root(&self, host_root: &Path) -> usize {
        let uris = self
            .documents
            .read()
            .await
            .iter()
            .filter(|(_, document)| document.host_path.starts_with(host_root))
            .map(|(uri, _)| uri.clone())
            .collect::<HashSet<_>>();
        if uris.is_empty() {
            return 0;
        }
        self.documents
            .write()
            .await
            .retain(|uri, _| !uris.contains(uri));
        self.inner
            .write()
            .await
            .retain(|uri, _| !uris.contains(uri));
        self.sent.write().await.retain(|uri, _| !uris.contains(uri));
        self.notify.notify_waiters();
        uris.len()
    }

    /// Snapshot every URI's diagnostics. Order is unspecified.
    pub async fn all_diagnostics(&self) -> Vec<(Url, Vec<Diagnostic>)> {
        self.inner
            .read()
            .await
            .iter()
            .map(|(uri, entry)| (uri.clone(), entry.entry.diagnostics.clone()))
            .collect()
    }

    /// Total number of URIs tracked.
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    /// Whether the registry is empty.
    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
    }

    /// New independent consumer cursor.
    #[must_use]
    pub fn consumer_cursor(&self) -> DiagnosticConsumerCursor {
        DiagnosticConsumerCursor::default()
    }

    #[must_use]
    pub fn diagnostics_source(
        &self,
        host_root: Option<PathBuf>,
        settle_timeout: Option<Duration>,
    ) -> Arc<dyn platform_api::NewDiagnosticsSource> {
        Arc::new(LspDiagnosticsSource {
            registry: self.clone(),
            cursor: Mutex::new(self.consumer_cursor()),
            host_root,
            settle_timeout,
        })
    }

    /// Compute a passive `<new-diagnostics>` block using an independent cursor.
    pub async fn take_new_diagnostics_block_for_cursor(
        &self,
        cursor: &mut DiagnosticConsumerCursor,
    ) -> Option<String> {
        self.take_new_diagnostics_block_for_cursor_under_host_root(cursor, None)
            .await
    }

    /// Compute a passive `<new-diagnostics>` block using an independent cursor,
    /// optionally restricted to one host root.
    pub async fn take_new_diagnostics_block_for_cursor_under_host_root(
        &self,
        cursor: &mut DiagnosticConsumerCursor,
        host_root: Option<&Path>,
    ) -> Option<String> {
        let snapshots = self.inner.read().await;
        let documents = self.documents.read().await;

        let mut uris: Vec<&Url> = snapshots.keys().collect();
        uris.sort_by(|a, b| a.as_str().cmp(b.as_str()));

        let mut files: Vec<DiagnosticFile> = Vec::new();
        for uri in uris {
            if let Some(root) = host_root {
                let Some(document) = documents.get(uri) else {
                    continue;
                };
                if !document.host_path.starts_with(root) {
                    continue;
                }
            }
            let Some(snapshot) = snapshots.get(uri) else {
                continue;
            };
            let seen = cursor.seen.entry(uri.clone()).or_default();
            let mut fresh: Vec<Diagnostic> = Vec::new();
            for diagnostic in &snapshot.entry.diagnostics {
                let key = dedup_key(diagnostic);
                if seen.insert(key) {
                    fresh.push(diagnostic.clone());
                }
            }
            if !fresh.is_empty() {
                files.push(DiagnosticFile {
                    uri: uri.to_string(),
                    diagnostics: fresh,
                });
            }
        }

        if files.is_empty() {
            None
        } else {
            Some(format_diagnostics_block(&files))
        }
    }

    /// Latest diagnostics for documents under `host_root`.
    pub async fn diagnostics_under_host_root(
        &self,
        host_root: &Path,
    ) -> Vec<HostDiagnosticSnapshot> {
        let root = host_root.to_path_buf();
        let documents = self.documents.read().await;
        let snapshots = self.inner.read().await;
        let mut out = Vec::new();
        for (uri, document) in documents.iter() {
            if !document.host_path.starts_with(&root) {
                continue;
            }
            let Some(snapshot) = snapshots.get(uri) else {
                continue;
            };
            let freshness = match (document.last_synced_version, snapshot.entry.version) {
                (Some(expected), Some(actual)) if expected == actual => DiagnosticFreshness::Fresh,
                (Some(expected), Some(actual)) => {
                    DiagnosticFreshness::AdvisoryVersionMismatch { expected, actual }
                }
                (_, None) => DiagnosticFreshness::AdvisoryNoVersion,
                (None, Some(actual)) => DiagnosticFreshness::AdvisoryVersionMismatch {
                    expected: 0,
                    actual,
                },
            };
            out.push(HostDiagnosticSnapshot {
                host_path: document.host_path.clone(),
                server_path: document.server_path.clone(),
                uri: uri.clone(),
                synced_version: document.last_synced_version,
                diagnostics_version: snapshot.entry.version,
                freshness,
                diagnostics: snapshot.entry.diagnostics.clone(),
            });
        }
        out.sort_by(|left, right| left.host_path.cmp(&right.host_path));
        out
    }

    /// Wait until every tracked document under `host_root` has observed a
    /// `publishDiagnostics` after its latest sync, or the timeout expires.
    pub async fn settle_under_host_root(
        &self,
        host_root: &Path,
        timeout: Duration,
    ) -> DiagnosticSettleStatus {
        let root = host_root.to_path_buf();
        if self.pending_documents_under_root(&root).await == 0 {
            let tracked = self.tracked_documents_under_root(&root).await;
            return DiagnosticSettleStatus {
                state: if tracked == 0 {
                    DiagnosticSettleState::NoTrackedDocuments
                } else {
                    DiagnosticSettleState::Settled
                },
                tracked_documents: tracked,
            };
        }

        let wait = async {
            loop {
                let notified = self.notify.notified();
                if self.pending_documents_under_root(&root).await == 0 {
                    break;
                }
                notified.await;
            }
        };
        let tracked_documents = self.tracked_documents_under_root(&root).await;
        let state = match tokio::time::timeout(timeout, wait).await {
            Ok(()) => DiagnosticSettleState::Settled,
            Err(_) => DiagnosticSettleState::TimedOut,
        };
        DiagnosticSettleStatus {
            state,
            tracked_documents,
        }
    }

    async fn pending_documents_under_root(&self, host_root: &Path) -> usize {
        let documents = self.documents.read().await;
        let snapshots = self.inner.read().await;
        documents
            .iter()
            .filter(|(uri, document)| {
                document.host_path.starts_with(host_root)
                    && document.sync_generation > 0
                    && snapshots.get(*uri).is_none_or(|snapshot| {
                        snapshot.entry.version != document.last_synced_version
                    })
                    && document.publish_generation < document.sync_generation
            })
            .count()
    }

    async fn tracked_documents_under_root(&self, host_root: &Path) -> usize {
        self.documents
            .read()
            .await
            .values()
            .filter(|document| document.host_path.starts_with(host_root))
            .count()
    }

    /// Compute the passive `<new-diagnostics>` reminder for any diagnostics not
    /// yet surfaced to the model, marking them surfaced.
    pub async fn take_new_diagnostics_block(&self) -> Option<String> {
        let mut sent = self.sent.write().await;
        let mut cursor = DiagnosticConsumerCursor {
            seen: std::mem::take(&mut *sent),
        };
        let block = self
            .take_new_diagnostics_block_for_cursor(&mut cursor)
            .await;
        *sent = cursor.seen;
        block
    }
}

#[async_trait::async_trait]
impl platform_api::NewDiagnosticsSource for LspDiagnosticRegistry {
    async fn take_new_diagnostics_block(&self) -> Option<String> {
        LspDiagnosticRegistry::take_new_diagnostics_block(self).await
    }
}

#[async_trait::async_trait]
impl platform_api::NewDiagnosticsSource for LspDiagnosticsSource {
    async fn take_new_diagnostics_block(&self) -> Option<String> {
        if let (Some(root), Some(timeout)) = (&self.host_root, self.settle_timeout) {
            let _ = self.registry.settle_under_host_root(root, timeout).await;
        }
        let mut cursor = self.cursor.lock().await;
        self.registry
            .take_new_diagnostics_block_for_cursor_under_host_root(
                &mut cursor,
                self.host_root.as_deref(),
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{DiagnosticSeverity, Position, Range};
    use std::path::PathBuf;

    fn err(line: u32, msg: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(line, 0), Position::new(line, 1)),
            severity: Some(DiagnosticSeverity::ERROR),
            message: msg.to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn surfaces_new_once_then_skips_until_new_arrive() {
        let reg = LspDiagnosticRegistry::new();
        let uri = Url::parse("file:///repo/a.ts").unwrap();

        assert!(reg.take_new_diagnostics_block().await.is_none());

        reg.publish(
            uri.clone(),
            DiagnosticEntry {
                version: None,
                diagnostics: vec![err(0, "e1")],
            },
        )
        .await;
        let block = reg.take_new_diagnostics_block().await.expect("a block");
        assert!(block.contains("<new-diagnostics>"));
        assert!(block.contains("a.ts:"));
        assert!(block.contains("[Line 1:1] e1"));

        assert!(reg.take_new_diagnostics_block().await.is_none());

        reg.publish(
            uri.clone(),
            DiagnosticEntry {
                version: None,
                diagnostics: vec![err(0, "e1"), err(1, "e2")],
            },
        )
        .await;
        let block = reg.take_new_diagnostics_block().await.expect("e2 block");
        assert!(block.contains("e2"), "got: {block}");
        assert!(!block.contains("e1"), "e1 already surfaced: {block}");
        assert!(reg.take_new_diagnostics_block().await.is_none());
    }

    #[tokio::test]
    async fn clear_under_host_root_preserves_other_workspace_diagnostics() {
        let reg = LspDiagnosticRegistry::new();
        let root_a = PathBuf::from("/apps/a");
        let root_b = PathBuf::from("/apps/b");
        let uri_a = Url::parse("file:///workspace/a/app.js").unwrap();
        let uri_b = Url::parse("file:///workspace/b/app.js").unwrap();
        reg.record_document_sync(
            &root_a.join("app.js"),
            Path::new("/workspace/a/app.js"),
            uri_a.clone(),
            Some(1),
            true,
        )
        .await;
        reg.record_document_sync(
            &root_b.join("app.js"),
            Path::new("/workspace/b/app.js"),
            uri_b.clone(),
            Some(1),
            true,
        )
        .await;
        reg.publish(
            uri_a,
            DiagnosticEntry {
                version: Some(1),
                diagnostics: vec![err(0, "a")],
            },
        )
        .await;
        reg.publish(
            uri_b,
            DiagnosticEntry {
                version: Some(1),
                diagnostics: vec![err(0, "b")],
            },
        )
        .await;

        assert_eq!(reg.clear_under_host_root(&root_a).await, 1);
        assert!(reg.diagnostics_under_host_root(&root_a).await.is_empty());
        let remaining = reg.diagnostics_under_host_root(&root_b).await;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].diagnostics[0].message, "b");
    }

    #[tokio::test]
    async fn independent_cursors_do_not_consume_each_other() {
        let reg = LspDiagnosticRegistry::new();
        let uri = Url::parse("file:///repo/b.ts").unwrap();
        reg.publish(
            uri,
            DiagnosticEntry {
                version: Some(1),
                diagnostics: vec![err(0, "e1")],
            },
        )
        .await;

        let mut first = reg.consumer_cursor();
        let mut second = reg.consumer_cursor();
        assert!(reg
            .take_new_diagnostics_block_for_cursor(&mut first)
            .await
            .expect("first cursor")
            .contains("e1"));
        assert!(reg
            .take_new_diagnostics_block_for_cursor(&mut second)
            .await
            .expect("second cursor")
            .contains("e1"));
        assert!(reg
            .take_new_diagnostics_block_for_cursor(&mut first)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn settle_under_host_root_observes_publish_without_missing_notify() {
        let reg = LspDiagnosticRegistry::new();
        let root = PathBuf::from("/repo");
        let host_path = root.join("app/main.jsx");
        let server_path = PathBuf::from("/workspace/app/main.jsx");
        let uri = Url::parse("file:///workspace/app/main.jsx").unwrap();

        reg.record_document_sync(&host_path, &server_path, uri.clone(), Some(7), true)
            .await;

        let waiting = tokio::spawn({
            let reg = reg.clone();
            let root = root.clone();
            async move {
                reg.settle_under_host_root(&root, Duration::from_millis(200))
                    .await
            }
        });
        tokio::task::yield_now().await;
        reg.publish(
            uri,
            DiagnosticEntry {
                version: Some(7),
                diagnostics: vec![],
            },
        )
        .await;

        let status = waiting.await.expect("settle task");
        assert_eq!(status.state, DiagnosticSettleState::Settled);
        assert_eq!(status.tracked_documents, 1);
    }
}
