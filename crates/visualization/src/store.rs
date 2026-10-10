//! Per-root-session storage for published revisions and confirmed state.
//!
//! ```text
//! <config-home>/visualizations/<root-session-uuid>/
//!   index.json                 # id → title, revisions, agent_id, created_at
//!   revisions/<id>/<rev>.html  # author fragment, immutable
//!   state/<id>/<rev>.json      # confirmed widget state with its version
//! ```
//!
//! Every write goes through [`FileSystem::write_file_rooted_atomic`] (staged,
//! synced, renamed) and is acknowledged only after it returns. Writers for one
//! session are serialized in-process and across processes by an advisory lock.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use lingxi_core::host::{FileSystem, FsError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::checks::MAX_FRAGMENT_BYTES;
use crate::document::BootState;
use crate::reference::{VisualizationId, VisualizationRef};

/// Largest confirmed state: the two JSON payloads together, in UTF-8 bytes.
pub const MAX_STATE_BYTES: usize = 16 * 1024;
/// Sum of fragment bytes one root session may hold.
pub const MAX_SESSION_BYTES: u64 = 64 * 1024 * 1024;
/// Revisions one visualization id may accumulate.
pub const MAX_REVISIONS_PER_ID: usize = 50;
/// Longest title, in characters.
pub const MAX_TITLE_CHARS: usize = 80;

const INDEX: &str = "index.json";
const LOCK: &str = ".lock";
const INDEX_SCHEMA: u32 = 1;

/// Store failures, phrased for the agent or the host.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StoreError {
    /// No such session, id or revision in this root session.
    #[error("visualization {0} is not available in this conversation")]
    NotFound(String),
    /// A quota was reached.
    #[error("{0}")]
    Quota(String),
    /// Input rejected before touching disk.
    #[error("{0}")]
    Invalid(String),
    /// Underlying filesystem failure.
    #[error("visualization storage failed: {0}")]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_id: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateFile {
    version: u64,
    model_content: serde_json::Value,
    private_content: serde_json::Value,
}

/// Who publishes a revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publisher {
    /// Root session that owns the visualization.
    pub root_session: Uuid,
    /// Subagent that published it, when not the main thread.
    pub agent_id: Option<String>,
}

/// A newly stored revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    /// The revision to reference.
    pub reference: VisualizationRef,
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
    /// Publishing subagent, if any.
    pub agent_id: Option<String>,
}

/// One visualization id with its revisions, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualizationSummary {
    /// Visualization id.
    pub id: VisualizationId,
    /// Revisions, oldest first.
    pub revisions: Vec<RevisionSummary>,
}

/// A revision read for rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRevision {
    /// Title.
    pub title: String,
    /// Author fragment.
    pub fragment: String,
}

/// Result of a compare-and-swap state write.
#[derive(Debug, Clone, PartialEq)]
pub enum StateWrite {
    /// Durable at this version.
    Saved {
        /// New version.
        version: u64,
    },
    /// `base_version` was stale; nothing was written.
    Conflict {
        /// The state that won.
        current: BootState,
    },
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

fn parse_state_json(label: &str, json: &str) -> Result<serde_json::Value, StoreError> {
    serde_json::from_str(json)
        .map_err(|error| StoreError::Invalid(format!("{label} is not valid JSON: {error}")))
}

/// The visualization store at `<config-home>/visualizations`.
///
/// Every access is rooted at the config home, which already exists, so the
/// rooted no-follow filesystem calls create the store directories on first
/// write and never traverse a symlink below the config home.
pub struct VisualizationStore {
    fs: Arc<dyn FileSystem>,
    config_home: PathBuf,
    writers: std::sync::Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>,
}

impl VisualizationStore {
    /// The store of `config_home`.
    #[must_use]
    pub fn new(fs: Arc<dyn FileSystem>, config_home: PathBuf) -> Self {
        Self {
            fs,
            config_home,
            writers: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// `<config-home>/visualizations`.
    #[must_use]
    pub fn root(&self) -> PathBuf {
        self.config_home.join(branding::VISUALIZATIONS_DIR)
    }

    /// Directory of one root session.
    #[must_use]
    pub fn session_dir(&self, session: Uuid) -> PathBuf {
        self.config_home.join(Self::session_relative(session))
    }

    fn session_relative(session: Uuid) -> PathBuf {
        Path::new(branding::VISUALIZATIONS_DIR).join(session.hyphenated().to_string())
    }

    fn relative(session: Uuid, tail: &Path) -> PathBuf {
        Self::session_relative(session).join(tail)
    }

    fn writer(&self, session: Uuid) -> Arc<tokio::sync::Mutex<()>> {
        let mut writers = self
            .writers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        writers.entry(session).or_default().clone()
    }

    async fn read_text(&self, session: Uuid, relative: &Path) -> Result<String, FsError> {
        self.fs
            .read_file_rooted_no_follow(&self.config_home, &Self::relative(session, relative))
            .await
            .map(|file| file.content)
    }

    async fn read_index(&self, session: Uuid) -> Result<Index, StoreError> {
        match self.read_text(session, Path::new(INDEX)).await {
            Ok(content) => serde_json::from_str::<Index>(&content)
                .map_err(|error| StoreError::Io(format!("corrupt {INDEX}: {error}"))),
            Err(FsError::NotFound(_)) => Ok(Index::default()),
            Err(error) => Err(error.into()),
        }
    }

    async fn write_json<T: Serialize>(
        &self,
        session: Uuid,
        relative: &Path,
        value: &T,
    ) -> Result<(), StoreError> {
        let json =
            serde_json::to_string(value).map_err(|error| StoreError::Io(error.to_string()))?;
        self.fs
            .write_file_rooted_atomic(&self.config_home, &Self::relative(session, relative), &json)
            .await?;
        Ok(())
    }

    fn revision_path(id: &VisualizationId, revision: u32) -> PathBuf {
        Path::new("revisions")
            .join(id.as_str())
            .join(format!("{revision}.html"))
    }

    fn state_path(id: &VisualizationId, revision: u32) -> PathBuf {
        Path::new("state")
            .join(id.as_str())
            .join(format!("{revision}.json"))
    }

    /// Store `fragment` as the next revision of `id` (a fresh id when `None`).
    ///
    /// # Errors
    /// Quota, validation or filesystem failures; nothing is acknowledged
    /// before the fragment and the index are durable.
    pub async fn publish(
        &self,
        publisher: &Publisher,
        id: Option<&VisualizationId>,
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
        let session = publisher.root_session;
        let writer = self.writer(session);
        let _serial = writer.lock().await;
        let _lock = self
            .fs
            .flock_exclusive_rooted(&self.config_home, &Self::relative(session, Path::new(LOCK)))
            .await?;
        let mut index = self.read_index(session).await?;
        let id = match id {
            Some(id) => {
                if !index.items.contains_key(id.as_str()) {
                    return Err(StoreError::NotFound(format!(
                        "id \"{id}\" (omit id to publish a new visualization)"
                    )));
                }
                id.clone()
            }
            None => loop {
                let candidate = format!("v{}", &Uuid::new_v4().simple().to_string()[..10]);
                if !index.items.contains_key(&candidate) {
                    break VisualizationId::parse(&candidate).expect("generated id is valid");
                }
            },
        };
        let bytes = fragment.len() as u64;
        if index.total_bytes.saturating_add(bytes) > MAX_SESSION_BYTES {
            return Err(StoreError::Quota(format!(
                "this conversation already stores {} bytes of visualizations; the limit is {MAX_SESSION_BYTES}",
                index.total_bytes
            )));
        }
        let item = index.items.entry(id.as_str().to_string()).or_default();
        if item.revisions.len() >= MAX_REVISIONS_PER_ID {
            return Err(StoreError::Quota(format!(
                "visualization \"{id}\" already has {MAX_REVISIONS_PER_ID} revisions; publish a new visualization instead"
            )));
        }
        let revision = item.revisions.last().map_or(1, |last| last.rev + 1);
        self.fs
            .write_file_rooted_atomic(
                &self.config_home,
                &Self::relative(session, &Self::revision_path(&id, revision)),
                fragment,
            )
            .await?;
        item.revisions.push(RevisionMeta {
            rev: revision,
            title: title.clone(),
            bytes,
            created_at_ms: now_ms,
            agent_id: publisher.agent_id.clone(),
        });
        index.total_bytes += bytes;
        self.write_json(session, Path::new(INDEX), &index).await?;
        Ok(Published {
            reference: VisualizationRef { id, revision },
            title,
        })
    }

    fn find_meta<'a>(index: &'a Index, reference: &VisualizationRef) -> Option<&'a RevisionMeta> {
        index
            .items
            .get(reference.id.as_str())?
            .revisions
            .iter()
            .find(|meta| meta.rev == reference.revision)
    }

    /// Read one revision for rendering, authorized by its root session.
    ///
    /// # Errors
    /// [`StoreError::NotFound`] for a forged, foreign or swept reference.
    pub async fn read_revision(
        &self,
        session: Uuid,
        reference: &VisualizationRef,
    ) -> Result<StoredRevision, StoreError> {
        let index = self.read_index(session).await?;
        let meta = Self::find_meta(&index, reference)
            .ok_or_else(|| StoreError::NotFound(reference.reference_line()))?;
        let fragment = self
            .read_text(
                session,
                &Self::revision_path(&reference.id, reference.revision),
            )
            .await
            .map_err(|_| StoreError::NotFound(reference.reference_line()))?;
        Ok(StoredRevision {
            title: meta.title.clone(),
            fragment,
        })
    }

    /// Confirmed state of a revision (version 0 and nulls before any save).
    ///
    /// # Errors
    /// [`StoreError::NotFound`] when the revision is not in this session.
    pub async fn read_state(
        &self,
        session: Uuid,
        reference: &VisualizationRef,
    ) -> Result<BootState, StoreError> {
        let index = self.read_index(session).await?;
        if Self::find_meta(&index, reference).is_none() {
            return Err(StoreError::NotFound(reference.reference_line()));
        }
        self.read_state_file(session, reference).await
    }

    async fn read_state_file(
        &self,
        session: Uuid,
        reference: &VisualizationRef,
    ) -> Result<BootState, StoreError> {
        match self
            .read_text(
                session,
                &Self::state_path(&reference.id, reference.revision),
            )
            .await
        {
            Ok(content) => {
                let state: StateFile = serde_json::from_str(&content)
                    .map_err(|error| StoreError::Io(format!("corrupt state: {error}")))?;
                Ok(BootState {
                    version: state.version,
                    model_content: state.model_content,
                    private_content: state.private_content,
                })
            }
            Err(FsError::NotFound(_)) => Ok(BootState::default()),
            Err(error) => Err(error.into()),
        }
    }

    /// Compare-and-swap the confirmed state; durable before `Saved` returns.
    ///
    /// # Errors
    /// Size or JSON validation, unknown revision, or filesystem failure.
    pub async fn write_state(
        &self,
        session: Uuid,
        reference: &VisualizationRef,
        base_version: u64,
        model_content_json: &str,
        private_content_json: &str,
    ) -> Result<StateWrite, StoreError> {
        if model_content_json.len() + private_content_json.len() > MAX_STATE_BYTES {
            return Err(StoreError::Quota(format!(
                "widget state exceeds {MAX_STATE_BYTES} bytes"
            )));
        }
        let model_content = parse_state_json("modelContent", model_content_json)?;
        let private_content = parse_state_json("privateContent", private_content_json)?;
        let writer = self.writer(session);
        let _serial = writer.lock().await;
        let _lock = self
            .fs
            .flock_exclusive_rooted(&self.config_home, &Self::relative(session, Path::new(LOCK)))
            .await?;
        let index = self.read_index(session).await?;
        if Self::find_meta(&index, reference).is_none() {
            return Err(StoreError::NotFound(reference.reference_line()));
        }
        let current = self.read_state_file(session, reference).await?;
        if current.version != base_version {
            return Ok(StateWrite::Conflict { current });
        }
        let version = current.version + 1;
        self.write_json(
            session,
            &Self::state_path(&reference.id, reference.revision),
            &StateFile {
                version,
                model_content,
                private_content,
            },
        )
        .await?;
        Ok(StateWrite::Saved { version })
    }

    /// Every visualization of a root session.
    ///
    /// # Errors
    /// Filesystem failure or a corrupt index.
    pub async fn list(&self, session: Uuid) -> Result<Vec<VisualizationSummary>, StoreError> {
        let index = self.read_index(session).await?;
        Ok(index
            .items
            .iter()
            .filter_map(|(id, item)| {
                Some(VisualizationSummary {
                    id: VisualizationId::parse(id)?,
                    revisions: item
                        .revisions
                        .iter()
                        .map(|meta| RevisionSummary {
                            revision: meta.rev,
                            title: meta.title.clone(),
                            bytes: meta.bytes,
                            created_at_ms: meta.created_at_ms,
                            agent_id: meta.agent_id.clone(),
                        })
                        .collect(),
                })
            })
            .collect())
    }

    /// Copy every revision and confirmed state of `from` into the empty root
    /// session `to`, keeping ids; afterwards the two diverge independently.
    ///
    /// # Errors
    /// Filesystem failure. A target that already holds visualizations is left
    /// untouched.
    pub async fn copy_session(&self, from: Uuid, to: Uuid) -> Result<usize, StoreError> {
        if from == to {
            return Ok(0);
        }
        let source = self.read_index(from).await?;
        if source.items.is_empty() {
            return Ok(0);
        }
        let writer = self.writer(to);
        let _serial = writer.lock().await;
        let _lock = self
            .fs
            .flock_exclusive_rooted(&self.config_home, &Self::relative(to, Path::new(LOCK)))
            .await?;
        if !self.read_index(to).await?.items.is_empty() {
            return Ok(0);
        }
        let mut copied = 0;
        for (id, item) in &source.items {
            let Some(id) = VisualizationId::parse(id) else {
                continue;
            };
            for meta in &item.revisions {
                let reference = VisualizationRef {
                    id: id.clone(),
                    revision: meta.rev,
                };
                let fragment = self.read_revision(from, &reference).await?.fragment;
                self.fs
                    .write_file_rooted_atomic(
                        &self.config_home,
                        &Self::relative(to, &Self::revision_path(&id, meta.rev)),
                        &fragment,
                    )
                    .await?;
                let state = self.read_state_file(from, &reference).await?;
                if state.version > 0 {
                    self.write_json(
                        to,
                        &Self::state_path(&id, meta.rev),
                        &StateFile {
                            version: state.version,
                            model_content: state.model_content,
                            private_content: state.private_content,
                        },
                    )
                    .await?;
                }
                copied += 1;
            }
        }
        self.write_json(to, Path::new(INDEX), &source).await?;
        Ok(copied)
    }

    /// Remove every file the index of `session` names, then the index.
    /// Directory removal is left to the host's maintenance sweep.
    ///
    /// # Errors
    /// Filesystem failure other than already-missing files.
    pub async fn delete_session(&self, session: Uuid) -> Result<(), StoreError> {
        let writer = self.writer(session);
        let _serial = writer.lock().await;
        let index = self.read_index(session).await?;
        for (id, item) in &index.items {
            let Some(id) = VisualizationId::parse(id) else {
                continue;
            };
            for meta in &item.revisions {
                for relative in [
                    Self::revision_path(&id, meta.rev),
                    Self::state_path(&id, meta.rev),
                ] {
                    match self
                        .fs
                        .delete_file_rooted_no_follow(
                            &self.config_home,
                            &Self::relative(session, &relative),
                        )
                        .await
                    {
                        Ok(()) | Err(FsError::NotFound(_)) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
            }
        }
        match self
            .fs
            .delete_file_rooted_no_follow(
                &self.config_home,
                &Self::relative(session, Path::new(INDEX)),
            )
            .await
        {
            Ok(()) | Err(FsError::NotFound(_)) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, VisualizationStore) {
        let dir = tempfile::tempdir().unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::PosixFileSystem::new(
            dir.path().to_path_buf(),
        ));
        let store = VisualizationStore::new(fs, dir.path().to_path_buf());
        (dir, store)
    }

    fn publisher(session: Uuid) -> Publisher {
        Publisher {
            root_session: session,
            agent_id: None,
        }
    }

    #[tokio::test]
    async fn publish_read_and_list_round_trip() {
        let (_dir, store) = store();
        let session = Uuid::new_v4();
        let first = store
            .publish(
                &publisher(session),
                None,
                "  Sales   by region ",
                "<div>v1</div>",
                10,
            )
            .await
            .unwrap();
        assert_eq!(first.reference.revision, 1);
        assert_eq!(first.title, "Sales by region");
        let second = store
            .publish(
                &Publisher {
                    root_session: session,
                    agent_id: Some("agent:x".into()),
                },
                Some(&first.reference.id),
                "Sales v2",
                "<div>v2</div>",
                20,
            )
            .await
            .unwrap();
        assert_eq!(second.reference.revision, 2);
        assert_eq!(
            store
                .read_revision(session, &first.reference)
                .await
                .unwrap()
                .fragment,
            "<div>v1</div>"
        );
        let revision = store
            .read_revision(session, &second.reference)
            .await
            .unwrap();
        assert_eq!(
            (revision.title.as_str(), revision.fragment.as_str()),
            ("Sales v2", "<div>v2</div>")
        );
        let listed = store.list(session).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].revisions.len(), 2);
        assert_eq!(listed[0].revisions[1].agent_id.as_deref(), Some("agent:x"));
    }

    #[tokio::test]
    async fn foreign_forged_and_unknown_references_are_not_found() {
        let (_dir, store) = store();
        let owner = Uuid::new_v4();
        let published = store
            .publish(&publisher(owner), None, "t", "<p>x</p>", 0)
            .await
            .unwrap();
        let other = Uuid::new_v4();
        assert!(matches!(
            store.read_revision(other, &published.reference).await,
            Err(StoreError::NotFound(_))
        ));
        let forged = VisualizationRef {
            id: published.reference.id.clone(),
            revision: 9,
        };
        assert!(matches!(
            store.read_revision(owner, &forged).await,
            Err(StoreError::NotFound(_))
        ));
        let unknown = VisualizationId::parse("nope").unwrap();
        assert!(matches!(
            store
                .publish(&publisher(owner), Some(&unknown), "t", "<p>x</p>", 0)
                .await,
            Err(StoreError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn state_is_compare_and_swap() {
        let (_dir, store) = store();
        let session = Uuid::new_v4();
        let reference = store
            .publish(&publisher(session), None, "t", "<p>x</p>", 0)
            .await
            .unwrap()
            .reference;
        assert_eq!(
            store.read_state(session, &reference).await.unwrap(),
            BootState::default()
        );
        assert_eq!(
            store
                .write_state(session, &reference, 0, "{\"n\":1}", "null")
                .await
                .unwrap(),
            StateWrite::Saved { version: 1 }
        );
        let StateWrite::Conflict { current } = store
            .write_state(session, &reference, 0, "2", "null")
            .await
            .unwrap()
        else {
            panic!("stale base must conflict");
        };
        assert_eq!(current.version, 1);
        assert_eq!(current.model_content, serde_json::json!({"n": 1}));
        assert_eq!(
            store
                .write_state(session, &reference, 1, "2", "{\"p\":true}")
                .await
                .unwrap(),
            StateWrite::Saved { version: 2 }
        );
        let state = store.read_state(session, &reference).await.unwrap();
        assert_eq!(
            (state.version, state.private_content),
            (2, serde_json::json!({"p": true}))
        );
        let oversized = format!("\"{}\"", "x".repeat(MAX_STATE_BYTES));
        assert!(matches!(
            store
                .write_state(session, &reference, 2, &oversized, "null")
                .await,
            Err(StoreError::Quota(_))
        ));
        assert!(matches!(
            store.write_state(session, &reference, 2, "{", "null").await,
            Err(StoreError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn quotas_and_titles_are_enforced() {
        let (_dir, store) = store();
        let session = Uuid::new_v4();
        assert!(matches!(
            store
                .publish(&publisher(session), None, " ", "<p>x</p>", 0)
                .await,
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            store
                .publish(
                    &publisher(session),
                    None,
                    &"t".repeat(MAX_TITLE_CHARS + 1),
                    "<p>x</p>",
                    0
                )
                .await,
            Err(StoreError::Invalid(_))
        ));
        let id = store
            .publish(&publisher(session), None, "t", "<p>x</p>", 0)
            .await
            .unwrap()
            .reference
            .id;
        for _ in 1..MAX_REVISIONS_PER_ID {
            store
                .publish(&publisher(session), Some(&id), "t", "<p>x</p>", 0)
                .await
                .unwrap();
        }
        assert!(matches!(
            store
                .publish(&publisher(session), Some(&id), "t", "<p>x</p>", 0)
                .await,
            Err(StoreError::Quota(_))
        ));
    }

    #[tokio::test]
    async fn fork_copies_revisions_and_state_then_diverges() {
        let (_dir, store) = store();
        let parent = Uuid::new_v4();
        let reference = store
            .publish(&publisher(parent), None, "t", "<p>x</p>", 0)
            .await
            .unwrap()
            .reference;
        store
            .write_state(parent, &reference, 0, "1", "null")
            .await
            .unwrap();
        let child = Uuid::new_v4();
        assert_eq!(store.copy_session(parent, child).await.unwrap(), 1);
        assert_eq!(
            store
                .read_revision(child, &reference)
                .await
                .unwrap()
                .fragment,
            "<p>x</p>"
        );
        assert_eq!(
            store.read_state(child, &reference).await.unwrap().version,
            1
        );
        store
            .write_state(child, &reference, 1, "2", "null")
            .await
            .unwrap();
        assert_eq!(
            store.read_state(parent, &reference).await.unwrap().version,
            1
        );
        assert_eq!(
            store.copy_session(parent, child).await.unwrap(),
            0,
            "non-empty target is left alone"
        );
    }

    #[tokio::test]
    async fn delete_session_removes_every_indexed_file() {
        let (_dir, store) = store();
        let session = Uuid::new_v4();
        let reference = store
            .publish(&publisher(session), None, "t", "<p>x</p>", 0)
            .await
            .unwrap()
            .reference;
        store
            .write_state(session, &reference, 0, "1", "null")
            .await
            .unwrap();
        store.delete_session(session).await.unwrap();
        assert!(store.list(session).await.unwrap().is_empty());
        assert!(matches!(
            store.read_revision(session, &reference).await,
            Err(StoreError::NotFound(_))
        ));
        let dir = store.session_dir(session);
        assert!(!dir
            .join("revisions")
            .join(reference.id.as_str())
            .join("1.html")
            .exists());
        store.delete_session(session).await.unwrap();
    }
}
