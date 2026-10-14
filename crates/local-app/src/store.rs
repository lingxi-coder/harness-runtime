//! The device's app library.
//!
//! ```text
//! <config-home>/local-apps/
//!   index.json        # id → name, summary, kept revisions, created_at
//!   <id>/<rev>.html   # one immutable document per revision
//! ```
//!
//! Unlike visualizations, apps belong to the device rather than to a
//! conversation: they are listed, opened and deleted by id, and nothing sweeps
//! them. The newest revision is the app; the few before it stay on disk so a
//! publish never leaves the app without a document. Every write goes through
//! [`FileSystem::write_file_rooted_atomic`] (staged, synced, renamed) and
//! writers are serialized in-process and across processes by an advisory lock.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lingxi_core::host::{FileSystem, FsError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Largest document, in UTF-8 bytes: the market's single-file limit.
pub const MAX_DOCUMENT_BYTES: usize = 25 * 1024 * 1024;
/// Sum of the kept documents of every app.
pub const MAX_LIBRARY_BYTES: u64 = 512 * 1024 * 1024;
/// Apps one library may hold.
pub const MAX_APPS: usize = 200;
/// Revisions kept per app, the current one included.
pub const KEPT_REVISIONS: usize = 10;
/// Longest name, in characters (the market's limit).
pub const MAX_NAME_CHARS: usize = 60;
/// Longest summary, in characters (the market's limit).
pub const MAX_SUMMARY_CHARS: usize = 160;

const INDEX: &str = "index.json";
const LOCK: &str = ".lock";
const INDEX_SCHEMA: u32 = 1;

/// An app id. The store makes them (`a` + 10 hex digits); only lowercase
/// letters and digits are accepted, so a host can use the id as the hostname
/// label of the app's own origin.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalAppId(String);

impl LocalAppId {
    /// Longest id accepted.
    pub const MAX_LEN: usize = 32;

    /// `value` as an id, or `None` when it is not one.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let valid = !value.is_empty()
            && value.len() <= Self::MAX_LEN
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
        valid.then(|| Self(value.to_string()))
    }

    fn generate() -> Self {
        Self(format!("a{}", &Uuid::new_v4().simple().to_string()[..10]))
    }

    /// The id text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for LocalAppId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Store failures, phrased for the agent or the host.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StoreError {
    /// No such app in this library.
    #[error("local app {0} is not in this device's library")]
    NotFound(String),
    /// A quota was reached.
    #[error("{0}")]
    Quota(String),
    /// Input rejected before touching disk.
    #[error("{0}")]
    Invalid(String),
    /// Underlying filesystem failure.
    #[error("local app storage failed: {0}")]
    Io(String),
}

impl From<FsError> for StoreError {
    fn from(error: FsError) -> Self {
        Self::Io(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RevisionMeta {
    rev: u32,
    bytes: u64,
    created_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppEntry {
    name: String,
    #[serde(default)]
    summary: String,
    created_at_ms: u64,
    /// Kept revisions, oldest first; the last one is the app.
    revisions: Vec<RevisionMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Index {
    schema: u32,
    total_bytes: u64,
    apps: BTreeMap<String, AppEntry>,
}

impl Default for Index {
    fn default() -> Self {
        Self {
            schema: INDEX_SCHEMA,
            total_bytes: 0,
            apps: BTreeMap::new(),
        }
    }
}

/// A newly stored revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    /// The app.
    pub id: LocalAppId,
    /// The revision just stored, now the app's current one.
    pub revision: u32,
    /// The app's name after this publish.
    pub name: String,
}

/// One app as lists show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSummary {
    /// App id.
    pub id: LocalAppId,
    /// Display name.
    pub name: String,
    /// One line on what it does; may be empty.
    pub summary: String,
    /// Current revision.
    pub revision: u32,
    /// Size of the current document.
    pub bytes: u64,
    /// First publish, Unix milliseconds.
    pub created_at_ms: u64,
    /// Publish of the current revision, Unix milliseconds.
    pub updated_at_ms: u64,
}

/// An app's current document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredApp {
    /// The app.
    pub summary: AppSummary,
    /// Its current HTML document.
    pub html: String,
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Normalize and validate an app name.
///
/// # Errors
/// [`StoreError::Invalid`] when empty or longer than [`MAX_NAME_CHARS`].
pub fn normalize_name(name: &str) -> Result<String, StoreError> {
    let name = collapse(name);
    if name.is_empty() {
        return Err(StoreError::Invalid("name must not be empty".into()));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(StoreError::Invalid(format!(
            "name must be at most {MAX_NAME_CHARS} characters"
        )));
    }
    Ok(name)
}

/// Normalize and validate a summary; an empty one is allowed.
///
/// # Errors
/// [`StoreError::Invalid`] when longer than [`MAX_SUMMARY_CHARS`].
pub fn normalize_summary(summary: &str) -> Result<String, StoreError> {
    let summary = collapse(summary);
    if summary.chars().count() > MAX_SUMMARY_CHARS {
        return Err(StoreError::Invalid(format!(
            "summary must be at most {MAX_SUMMARY_CHARS} characters"
        )));
    }
    Ok(summary)
}

fn summarize(id: &str, entry: &AppEntry) -> Option<AppSummary> {
    let current = entry.revisions.last()?;
    Some(AppSummary {
        id: LocalAppId::parse(id)?,
        name: entry.name.clone(),
        summary: entry.summary.clone(),
        revision: current.rev,
        bytes: current.bytes,
        created_at_ms: entry.created_at_ms,
        updated_at_ms: current.created_at_ms,
    })
}

/// The app library at `<config-home>/local-apps`.
///
/// Every access is rooted at the config home, which already exists, so the
/// rooted no-follow filesystem calls create the library directories on first
/// write and never traverse a symlink below the config home.
pub struct LocalAppStore {
    fs: Arc<dyn FileSystem>,
    config_home: PathBuf,
    writer: tokio::sync::Mutex<()>,
}

impl LocalAppStore {
    /// The library of `config_home`.
    #[must_use]
    pub fn new(fs: Arc<dyn FileSystem>, config_home: PathBuf) -> Self {
        Self {
            fs,
            config_home,
            writer: tokio::sync::Mutex::new(()),
        }
    }

    /// `<config-home>/local-apps`.
    #[must_use]
    pub fn root(&self) -> PathBuf {
        self.config_home.join(branding::LOCAL_APPS_DIR)
    }

    fn relative(tail: &Path) -> PathBuf {
        Path::new(branding::LOCAL_APPS_DIR).join(tail)
    }

    fn document_path(id: &LocalAppId, revision: u32) -> PathBuf {
        Path::new(id.as_str()).join(format!("{revision}.html"))
    }

    async fn read_index(&self) -> Result<Index, StoreError> {
        match self
            .fs
            .read_file_rooted_no_follow(&self.config_home, &Self::relative(Path::new(INDEX)))
            .await
        {
            Ok(file) => serde_json::from_str::<Index>(&file.content)
                .map_err(|error| StoreError::Io(format!("corrupt {INDEX}: {error}"))),
            Err(FsError::NotFound(_)) => Ok(Index::default()),
            Err(error) => Err(error.into()),
        }
    }

    async fn write_index(&self, index: &Index) -> Result<(), StoreError> {
        let json =
            serde_json::to_string(index).map_err(|error| StoreError::Io(error.to_string()))?;
        self.fs
            .write_file_rooted_atomic(&self.config_home, &Self::relative(Path::new(INDEX)), &json)
            .await?;
        Ok(())
    }

    /// Remove documents the index no longer names. Runs after the index is
    /// durable, so a crash leaves at worst an orphan file, never an index
    /// entry without its document.
    async fn delete_documents(
        &self,
        id: &LocalAppId,
        revisions: &[RevisionMeta],
    ) -> Result<(), StoreError> {
        for meta in revisions {
            match self
                .fs
                .delete_file_rooted_no_follow(
                    &self.config_home,
                    &Self::relative(&Self::document_path(id, meta.rev)),
                )
                .await
            {
                Ok(()) | Err(FsError::NotFound(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    /// Store `html` as the next revision of `id`, or as a new app when `id`
    /// is `None` (which needs a `name`). A `None` name or summary keeps the
    /// app's current one. Only the newest [`KEPT_REVISIONS`] stay on disk.
    ///
    /// # Errors
    /// Validation, quota or filesystem failures; nothing is acknowledged
    /// before the document and the index are durable.
    pub async fn publish(
        &self,
        id: Option<&LocalAppId>,
        name: Option<&str>,
        summary: Option<&str>,
        html: &str,
        now_ms: u64,
    ) -> Result<Published, StoreError> {
        let name = name.map(normalize_name).transpose()?;
        let summary = summary.map(normalize_summary).transpose()?;
        if html.len() > MAX_DOCUMENT_BYTES {
            return Err(StoreError::Quota(format!(
                "the document is {} bytes; the limit is {MAX_DOCUMENT_BYTES} bytes (25 MiB)",
                html.len()
            )));
        }
        let _serial = self.writer.lock().await;
        let _lock = self
            .fs
            .flock_exclusive_rooted(&self.config_home, &Self::relative(Path::new(LOCK)))
            .await?;
        let mut index = self.read_index().await?;
        let id = match id {
            Some(id) => {
                if !index.apps.contains_key(id.as_str()) {
                    return Err(StoreError::NotFound(id.to_string()));
                }
                id.clone()
            }
            None => {
                let Some(name) = &name else {
                    return Err(StoreError::Invalid("a new app needs a name".into()));
                };
                if index.apps.len() >= MAX_APPS {
                    return Err(StoreError::Quota(format!(
                        "this device already holds {MAX_APPS} apps; delete one from the Apps page or publish a new revision of an existing app"
                    )));
                }
                let id = loop {
                    let candidate = LocalAppId::generate();
                    if !index.apps.contains_key(candidate.as_str()) {
                        break candidate;
                    }
                };
                index.apps.insert(
                    id.as_str().to_string(),
                    AppEntry {
                        name: name.clone(),
                        summary: String::new(),
                        created_at_ms: now_ms,
                        revisions: Vec::new(),
                    },
                );
                id
            }
        };
        let bytes = html.len() as u64;
        let entry = index
            .apps
            .get_mut(id.as_str())
            .expect("the entry exists or was just inserted");
        let pruned_count = (entry.revisions.len() + 1).saturating_sub(KEPT_REVISIONS);
        let freed: u64 = entry.revisions[..pruned_count]
            .iter()
            .map(|meta| meta.bytes)
            .sum();
        let total = index
            .total_bytes
            .saturating_sub(freed)
            .saturating_add(bytes);
        if total > MAX_LIBRARY_BYTES {
            return Err(StoreError::Quota(format!(
                "the app library already stores {} bytes; the limit is {MAX_LIBRARY_BYTES}",
                index.total_bytes
            )));
        }
        let revision = entry.revisions.last().map_or(1, |last| last.rev + 1);
        self.fs
            .write_file_rooted_atomic(
                &self.config_home,
                &Self::relative(&Self::document_path(&id, revision)),
                html,
            )
            .await?;
        if let Some(name) = name {
            entry.name = name;
        }
        if let Some(summary) = summary {
            entry.summary = summary;
        }
        entry.revisions.push(RevisionMeta {
            rev: revision,
            bytes,
            created_at_ms: now_ms,
        });
        let pruned: Vec<RevisionMeta> = entry.revisions.drain(..pruned_count).collect();
        let name = entry.name.clone();
        index.total_bytes = total;
        self.write_index(&index).await?;
        self.delete_documents(&id, &pruned).await?;
        Ok(Published { id, revision, name })
    }

    /// Every app, most recently updated first.
    ///
    /// # Errors
    /// Filesystem failure or a corrupt index.
    pub async fn list(&self) -> Result<Vec<AppSummary>, StoreError> {
        let index = self.read_index().await?;
        let mut apps: Vec<AppSummary> = index
            .apps
            .iter()
            .filter_map(|(id, entry)| summarize(id, entry))
            .collect();
        apps.sort_by(|a, b| {
            b.updated_at_ms
                .cmp(&a.updated_at_ms)
                .then_with(|| a.name.cmp(&b.name))
        });
        Ok(apps)
    }

    /// One app.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when the library has no such app.
    pub async fn get(&self, id: &LocalAppId) -> Result<AppSummary, StoreError> {
        let index = self.read_index().await?;
        index
            .apps
            .get(id.as_str())
            .and_then(|entry| summarize(id.as_str(), entry))
            .ok_or_else(|| StoreError::NotFound(id.to_string()))
    }

    /// An app with its current document.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when the library has no such app.
    pub async fn read_current(&self, id: &LocalAppId) -> Result<StoredApp, StoreError> {
        let summary = self.get(id).await?;
        let html = self
            .fs
            .read_file_rooted_no_follow(
                &self.config_home,
                &Self::relative(&Self::document_path(id, summary.revision)),
            )
            .await
            .map_err(|_| StoreError::NotFound(id.to_string()))?
            .content;
        Ok(StoredApp { summary, html })
    }

    /// Remove an app and every kept revision. `false` when it was not there.
    ///
    /// # Errors
    /// Filesystem failure.
    pub async fn delete(&self, id: &LocalAppId) -> Result<bool, StoreError> {
        let _serial = self.writer.lock().await;
        let _lock = self
            .fs
            .flock_exclusive_rooted(&self.config_home, &Self::relative(Path::new(LOCK)))
            .await?;
        let mut index = self.read_index().await?;
        let Some(entry) = index.apps.remove(id.as_str()) else {
            return Ok(false);
        };
        let freed: u64 = entry.revisions.iter().map(|meta| meta.bytes).sum();
        index.total_bytes = index.total_bytes.saturating_sub(freed);
        self.write_index(&index).await?;
        self.delete_documents(id, &entry.revisions).await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, LocalAppStore) {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::PosixFileSystem::new(
            dir.path().to_path_buf(),
        ));
        let store = LocalAppStore::new(fs, dir.path().to_path_buf());
        (dir, store)
    }

    const DOC: &str = "<!doctype html><title>x</title><p>v1</p>";

    #[tokio::test]
    async fn publish_read_and_list_round_trip() {
        let (_dir, store) = store();
        let first = store
            .publish(
                None,
                Some("  Water   tracker "),
                Some(" Log glasses "),
                DOC,
                10,
            )
            .await
            .unwrap();
        assert_eq!((first.revision, first.name.as_str()), (1, "Water tracker"));
        assert!(first.id.as_str().starts_with('a') && first.id.as_str().len() == 11);
        let second = store
            .publish(Some(&first.id), None, None, "<!doctype html><p>v2</p>", 20)
            .await
            .unwrap();
        assert_eq!(
            (second.revision, second.name.as_str()),
            (2, "Water tracker")
        );
        let current = store.read_current(&first.id).await.unwrap();
        assert_eq!(current.html, "<!doctype html><p>v2</p>");
        assert_eq!(current.summary.summary, "Log glasses");
        assert_eq!(
            (current.summary.created_at_ms, current.summary.updated_at_ms),
            (10, 20)
        );
        let other = store
            .publish(None, Some("Timer"), None, DOC, 30)
            .await
            .unwrap();
        let listed = store.list().await.unwrap();
        assert_eq!(
            listed.iter().map(|app| app.id.clone()).collect::<Vec<_>>(),
            [other.id, first.id],
            "most recently updated first"
        );
    }

    #[tokio::test]
    async fn new_apps_need_a_name_and_unknown_ids_are_not_minted() {
        let (_dir, store) = store();
        assert!(matches!(
            store.publish(None, None, None, DOC, 0).await,
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            store.publish(None, Some(" "), None, DOC, 0).await,
            Err(StoreError::Invalid(_))
        ));
        let unknown = LocalAppId::parse("nope").unwrap();
        assert!(matches!(
            store.publish(Some(&unknown), Some("x"), None, DOC, 0).await,
            Err(StoreError::NotFound(_))
        ));
        assert!(matches!(
            store.read_current(&unknown).await,
            Err(StoreError::NotFound(_))
        ));
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn only_the_newest_revisions_stay_on_disk() {
        let (dir, store) = store();
        let id = store
            .publish(None, Some("App"), None, "<!doctype html>1", 0)
            .await
            .unwrap()
            .id;
        for n in 2..=(KEPT_REVISIONS as u32 + 2) {
            store
                .publish(Some(&id), None, None, &format!("<!doctype html>{n}"), 0)
                .await
                .unwrap();
        }
        let app_dir = dir.path().join("local-apps").join(id.as_str());
        assert!(!app_dir.join("1.html").exists());
        assert!(!app_dir.join("2.html").exists());
        assert!(app_dir.join("3.html").exists());
        assert_eq!(std::fs::read_dir(&app_dir).unwrap().count(), KEPT_REVISIONS);
        let current = store.read_current(&id).await.unwrap();
        assert_eq!(current.summary.revision, KEPT_REVISIONS as u32 + 2);
        let index: Index = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("local-apps/index.json")).unwrap(),
        )
        .unwrap();
        let kept: u64 = index.apps[id.as_str()]
            .revisions
            .iter()
            .map(|meta| meta.bytes)
            .sum();
        assert_eq!(index.total_bytes, kept);
    }

    #[tokio::test]
    async fn limits_are_enforced() {
        let (_dir, store) = store();
        assert!(matches!(
            store
                .publish(None, Some(&"n".repeat(MAX_NAME_CHARS + 1)), None, DOC, 0)
                .await,
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            store
                .publish(
                    None,
                    Some("x"),
                    Some(&"s".repeat(MAX_SUMMARY_CHARS + 1)),
                    DOC,
                    0
                )
                .await,
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            store
                .publish(
                    None,
                    Some("x"),
                    None,
                    &"a".repeat(MAX_DOCUMENT_BYTES + 1),
                    0
                )
                .await,
            Err(StoreError::Quota(_))
        ));
    }

    #[tokio::test]
    async fn delete_removes_the_app_and_its_documents() {
        let (dir, store) = store();
        let id = store
            .publish(None, Some("App"), None, DOC, 0)
            .await
            .unwrap()
            .id;
        store.publish(Some(&id), None, None, DOC, 1).await.unwrap();
        assert!(store.delete(&id).await.unwrap());
        assert!(!store.delete(&id).await.unwrap());
        assert!(store.list().await.unwrap().is_empty());
        let app_dir = dir.path().join("local-apps").join(id.as_str());
        assert!(!app_dir.join("1.html").exists() && !app_dir.join("2.html").exists());
        let index: Index = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("local-apps/index.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(index.total_bytes, 0);
    }

    #[test]
    fn ids_are_hostname_safe() {
        assert!(LocalAppId::parse("a1b2c3").is_some());
        for bad in ["", "A1", "a_1", "a-1", "../x", "a.b", &"a".repeat(33)] {
            assert!(LocalAppId::parse(bad).is_none(), "{bad}");
        }
        let generated = LocalAppId::generate();
        assert_eq!(LocalAppId::parse(generated.as_str()), Some(generated));
    }
}
