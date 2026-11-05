//! Config-home storage for private pages.
//!
//! ```text
//! <config-home>/pages/
//!   index.json
//!   revisions/<id>/<rev>.html
//! ```
//!
//! Pages belong to this computer, not to one conversation. `list` returns
//! every page in this store. Writes are rooted, atomic, and acknowledged only
//! after they succeed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lingxi_core::host::{FileSystem, FsError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::checks::MAX_FRAGMENT_BYTES;
use crate::reference::{PageId, PageRef};

/// Revisions one page id may accumulate.
pub const MAX_REVISIONS_PER_ID: usize = 50;
/// Sum of fragment bytes this computer may hold.
pub const MAX_STORE_BYTES: u64 = 64 * 1024 * 1024;
/// Longest title, in characters.
pub const MAX_TITLE_CHARS: usize = 80;

const INDEX: &str = "index.json";
const LOCK: &str = ".lock";
const INDEX_SCHEMA: u32 = 1;

/// Store failures, phrased for the agent or the host.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StoreError {
    /// No such id or revision on this computer.
    #[error("private page {0} is not on this computer")]
    NotFound(String),
    /// A quota was reached.
    #[error("{0}")]
    Quota(String),
    /// Input rejected before touching disk.
    #[error("{0}")]
    Invalid(String),
    /// Underlying filesystem failure.
    #[error("private page storage failed: {0}")]
    Io(String),
}

impl From<FsError> for StoreError {
    fn from(error: FsError) -> Self {
        match error {
            FsError::NotFound(path) => Self::NotFound(path),
            other => Self::Io(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RevisionMeta {
    rev: u32,
    title: String,
    bytes: u64,
    created_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct IndexItem {
    revisions: Vec<RevisionMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Index {
    schema: u32,
    total_bytes: u64,
    items: BTreeMap<String, IndexItem>,
}

impl Default for Index {
    fn default() -> Self {
        Self {
            schema: INDEX_SCHEMA,
            total_bytes: 0,
            items: BTreeMap::new(),
        }
    }
}

/// A newly stored revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    /// The revision to reference.
    pub reference: PageRef,
    /// Its title.
    pub title: String,
}

/// One stored revision's metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionSummary {
    /// Revision number.
    pub revision: u32,
    /// Title at that revision.
    pub title: String,
    /// Fragment size in bytes.
    pub bytes: u64,
    /// Publish time.
    pub created_at_ms: u64,
}

/// One page id with its revisions, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageSummary {
    /// Page id.
    pub id: PageId,
    /// Revisions, oldest first.
    pub revisions: Vec<RevisionSummary>,
}

/// A revision read for rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPage {
    /// Title.
    pub title: String,
    /// Author fragment.
    pub fragment: String,
}

/// Normalize and validate a title.
///
/// # Errors
/// [`StoreError::Invalid`] when empty or longer than [`MAX_TITLE_CHARS`].
pub fn normalize_title(title: &str) -> Result<String, StoreError> {
    let title: String = title.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.is_empty() {
        return Err(StoreError::Invalid("title must not be empty".into()));
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(StoreError::Invalid(format!(
            "title must be at most {MAX_TITLE_CHARS} characters"
        )));
    }
    Ok(title)
}

/// The private-page store at `<config-home>/pages`.
pub struct PageStore {
    fs: Arc<dyn FileSystem>,
    config_home: PathBuf,
    writer: tokio::sync::Mutex<()>,
}

impl PageStore {
    /// The store of `config_home`.
    #[must_use]
    pub fn new(fs: Arc<dyn FileSystem>, config_home: PathBuf) -> Self {
        Self {
            fs,
            config_home,
            writer: tokio::sync::Mutex::new(()),
        }
    }

    fn relative(tail: &Path) -> PathBuf {
        Path::new(branding::PRIVATE_PAGES_DIR).join(tail)
    }

    fn revision_path(id: &PageId, revision: u32) -> PathBuf {
        Path::new("revisions")
            .join(id.as_str())
            .join(format!("{revision}.html"))
    }

    async fn read_text(&self, relative: &Path) -> Result<String, FsError> {
        self.fs
            .read_file_rooted_no_follow(&self.config_home, &Self::relative(relative))
            .await
            .map(|file| file.content)
    }

    async fn read_index(&self) -> Result<Index, StoreError> {
        match self.read_text(Path::new(INDEX)).await {
            Ok(content) => serde_json::from_str::<Index>(&content)
                .map_err(|error| StoreError::Io(format!("corrupt {INDEX}: {error}"))),
            Err(FsError::NotFound(_)) => Ok(Index::default()),
            Err(error) => Err(error.into()),
        }
    }

    async fn write_json<T: Serialize>(&self, relative: &Path, value: &T) -> Result<(), StoreError> {
        let json =
            serde_json::to_string(value).map_err(|error| StoreError::Io(error.to_string()))?;
        self.fs
            .write_file_rooted_atomic(&self.config_home, &Self::relative(relative), &json)
            .await?;
        Ok(())
    }

    /// Store `fragment` as the next revision of `id` (a fresh id when `None`).
    ///
    /// # Errors
    /// Quota, validation or filesystem failures. Nothing is acknowledged
    /// before the fragment and the index are durable.
    pub async fn publish(
        &self,
        id: Option<&PageId>,
        title: &str,
        fragment: &str,
        now_ms: u64,
    ) -> Result<Published, StoreError> {
        let title = normalize_title(title)?;
        if fragment.len() > MAX_FRAGMENT_BYTES {
            return Err(StoreError::Quota(format!(
                "the fragment exceeds {MAX_FRAGMENT_BYTES} bytes"
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
                if !index.items.contains_key(id.as_str()) {
                    return Err(StoreError::NotFound(format!(
                        "id \"{id}\" (omit id to publish a new page)"
                    )));
                }
                id.clone()
            }
            None => loop {
                let candidate = format!("p{}", &Uuid::new_v4().simple().to_string()[..10]);
                if !index.items.contains_key(&candidate) {
                    break PageId::parse(&candidate).expect("generated id is valid");
                }
            },
        };
        let bytes = u64::try_from(fragment.len()).unwrap_or(u64::MAX);
        if index.total_bytes.saturating_add(bytes) > MAX_STORE_BYTES {
            return Err(StoreError::Quota(format!(
                "this computer already stores {} bytes of private pages; the limit is {MAX_STORE_BYTES}",
                index.total_bytes
            )));
        }
        let item = index.items.entry(id.as_str().to_string()).or_default();
        if item.revisions.len() >= MAX_REVISIONS_PER_ID {
            return Err(StoreError::Quota(format!(
                "page \"{id}\" already has {MAX_REVISIONS_PER_ID} revisions; publish a new page instead"
            )));
        }
        let revision = item.revisions.last().map_or(1, |last| last.rev + 1);
        self.fs
            .write_file_rooted_atomic(
                &self.config_home,
                &Self::relative(&Self::revision_path(&id, revision)),
                fragment,
            )
            .await?;
        item.revisions.push(RevisionMeta {
            rev: revision,
            title: title.clone(),
            bytes,
            created_at_ms: now_ms,
        });
        index.total_bytes += bytes;
        self.write_json(Path::new(INDEX), &index).await?;
        Ok(Published {
            reference: PageRef { id, revision },
            title,
        })
    }

    /// Every page on this computer, oldest revision first within each id.
    ///
    /// # Errors
    /// The index cannot be read.
    pub async fn list(&self) -> Result<Vec<PageSummary>, StoreError> {
        let _serial = self.writer.lock().await;
        let index = self.read_index().await?;
        let mut pages = Vec::new();
        for (id, item) in index.items {
            let Some(id) = PageId::parse(&id) else {
                continue;
            };
            pages.push(PageSummary {
                id,
                revisions: item
                    .revisions
                    .into_iter()
                    .map(|meta| RevisionSummary {
                        revision: meta.rev,
                        title: meta.title,
                        bytes: meta.bytes,
                        created_at_ms: meta.created_at_ms,
                    })
                    .collect(),
            });
        }
        Ok(pages)
    }

    /// Read one revision for the host that opens it.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when the id or revision is absent.
    pub async fn read(&self, reference: &PageRef) -> Result<StoredPage, StoreError> {
        let _serial = self.writer.lock().await;
        let index = self.read_index().await?;
        let meta = index
            .items
            .get(reference.id.as_str())
            .and_then(|item| {
                item.revisions
                    .iter()
                    .find(|meta| meta.rev == reference.revision)
            })
            .ok_or_else(|| StoreError::NotFound(reference.reference_line()))?;
        let title = meta.title.clone();
        let fragment = self
            .read_text(&Self::revision_path(&reference.id, reference.revision))
            .await
            .map_err(|_| StoreError::NotFound(reference.reference_line()))?;
        Ok(StoredPage { title, fragment })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_store(home: &Path) -> PageStore {
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::PosixFileSystem::new(home.to_path_buf()));
        PageStore::new(fs, home.to_path_buf())
    }

    #[tokio::test]
    async fn publish_revises_and_lists_every_page_on_this_computer() {
        let home = tempfile::tempdir().unwrap();
        let store = open_store(home.path());
        let first = store
            .publish(None, "Notes", "<p>one</p>", 10)
            .await
            .unwrap();
        assert_eq!(first.reference.revision, 1);
        let second = store
            .publish(Some(&first.reference.id), "Notes", "<p>two</p>", 20)
            .await
            .unwrap();
        assert_eq!(second.reference.revision, 2);
        let stored = store.read(&second.reference).await.unwrap();
        assert_eq!(stored.fragment, "<p>two</p>");
        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].revisions.len(), 2);
        let elsewhere = tempfile::tempdir().unwrap();
        assert!(open_store(elsewhere.path())
            .list()
            .await
            .unwrap()
            .is_empty());
    }
}
