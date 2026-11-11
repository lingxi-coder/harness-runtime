//! `/rewind` file-history — per-file content backups keyed by turn, so the
//! working tree can be restored to a previous point. Faithful port of
//! claude-code's `utils/fileHistory.ts`.
//!
//! Flow: once per user turn the orchestrator calls [`FileHistory::make_snapshot`]
//! (a new snapshot capturing the current version of already-tracked files);
//! within the turn each Edit/Write/NotebookEdit tool calls
//! [`FileHistory::track_edit`] BEFORE it writes, backing up the file's pre-edit
//! content into that turn's snapshot. [`FileHistory::rewind_files`] restores the
//! tracked files to a target snapshot (restore / delete), which is what
//! `/rewind` drives.
//!
//! Backups live at `<lingxi_home>/file-history/<session>/<sha256(path)[:16]>@v<n>`;
//! `null` backup = the file did not exist at that version. The index is
//! persisted into the session JSONL as `file-history-snapshot` side-map lines so
//! `/rewind` survives `--resume`.
//!
//! Divergences from claude-code (documented, non-behavioral for the core): the
//! VSCode `file_updated` notifications and the resume-time hard-link backup copy
//! are omitted; only-once telemetry events are dropped.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use lingxi_core::host::{rooted_fs, FsError};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Above this the oldest snapshots are evicted (claude-code `MAX_SNAPSHOTS`).
const MAX_SNAPSHOTS: usize = 100;

/// One file's backup at a given version. `backup_file_name = None` marks "the
/// file did not exist in this version" (so rewinding deletes it).
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(test), derive(Clone))]
pub struct FileHistoryBackup {
    /// The `<hash>@v<n>` backup file name, or `None` if the file was absent.
    pub backup_file_name: Option<String>,
    /// Monotonic per-file version.
    pub version: u32,
    /// The original existing parent (or nearest existing ancestor) and its
    /// physical identity at the time the file was first tracked.
    pub source: FileHistorySource,
}

#[cfg(test)]
std::thread_local! {
    // Count owned backup copies mechanically, without wall-clock assertions.
    static BACKUP_CLONE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
impl Clone for FileHistoryBackup {
    fn clone(&self) -> Self {
        BACKUP_CLONE_COUNT.with(|count| count.set(count.get() + 1));
        Self {
            backup_file_name: self.backup_file_name.clone(),
            version: self.version,
            source: self.source.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileHistorySource {
    root: PathBuf,
    root_identity: rooted_fs::RootIdentity,
}

impl FileHistorySource {
    fn capture(path: &Path) -> Result<Self, FsError> {
        let mut root = path
            .parent()
            .ok_or_else(|| FsError::OutsideWorkspace(path.display().to_string()))?;
        loop {
            match std::fs::symlink_metadata(root) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => break,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    root = root
                        .parent()
                        .ok_or_else(|| FsError::NotFound(path.display().to_string()))?;
                }
                _ => return Err(FsError::OutsideWorkspace(root.display().to_string())),
            }
        }
        let root_identity = rooted_fs::root_identity(root)?;
        let source = Self {
            root: root.to_path_buf(),
            root_identity,
        };
        source.verify(path)?;
        Ok(source)
    }

    fn relative<'a>(&self, path: &'a Path) -> Result<&'a Path, FsError> {
        let relative = path
            .strip_prefix(&self.root)
            .map_err(|_| FsError::OutsideWorkspace(path.display().to_string()))?;
        rooted_fs::validate_relative_path(relative)?;
        Ok(relative)
    }

    fn has_original_parent(&self, path: &Path) -> bool {
        path.parent() == Some(self.root.as_path())
    }

    fn verify(&self, path: &Path) -> Result<(), FsError> {
        self.relative(path)?;
        // An identity check alone would accept a symlink leading back to the
        // original inode. Require the complete recorded parent chain to remain
        // canonical as well, then perform I/O through pinned no-follow handles.
        if std::fs::canonicalize(&self.root).ok().as_deref() != Some(self.root.as_path())
            || rooted_fs::root_identity(&self.root)? != self.root_identity
        {
            return Err(FsError::OutsideWorkspace(self.root.display().to_string()));
        }
        Ok(())
    }

    fn open(&self, path: &Path) -> Result<Option<std::fs::File>, FsError> {
        self.verify(path)?;
        let file = match rooted_fs::open_read_file_pinned(
            &self.root,
            self.relative(path)?,
            Some(&self.root_identity),
        ) {
            Ok(file) => file,
            Err(FsError::NotFound(_)) => {
                self.verify(path)?;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        rooted_fs::validate_open_file_single_link(&file)?;
        self.verify(path)?;
        Ok(Some(file))
    }
}

/// What a rewind actually did to the working tree.
///
/// claude-code's `rewindFiles` result (oracle schema `KNe`) is
/// `{canRewind, error?, filesChanged?, insertions?, deletions?, skippedLinks?}`.
/// The two fields that matter to a caller deciding whether to claim success are
/// [`Self::changed`] and [`Self::skipped_links`]; `canRewind:false` is spelled
/// here as `Err` from [`FileHistory::rewind_files`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RewindOutcome {
    /// Tracked paths actually restored or deleted.
    pub changed: Vec<String>,
    /// Count of tracked files NOT restored or deleted because a symlink, hard
    /// link, or other non-regular file was detected at the tracked path, or its
    /// backup could not be safely read.
    ///
    /// Oracle `skippedLinks` (2.1.216). Per its own description, other per-file
    /// failures — a missing backup file above all — are deliberately NOT counted
    /// here; they surface through [`Self::failed`] and, when every differing file
    /// fails, through the `Err` return.
    pub skipped_links: usize,
    /// Tracked files that needed a change and did not get one for a reason other
    /// than link safety (the missing-backup case). Not part of the oracle's wire
    /// shape — it is what decides the all-failed `canRewind:false` arm.
    pub failed: usize,
}

/// Per-file result of one rewind step, folded into [`RewindOutcome`].
enum RewindStep {
    /// The file already matched the target version — nothing to do.
    Unchanged,
    /// Restored from backup, or deleted because it was absent at the target.
    Changed(String),
    /// Refused: a symlink / hard link / non-regular file sits at the path.
    SkippedLink,
    /// Needed a change but could not be made (e.g. the backup file is gone).
    Failed,
}

/// A per-turn snapshot: the message it is keyed by + the pre-turn backups of
/// every tracked file.
#[derive(Debug, Clone)]
pub struct FileHistorySnapshot {
    /// The user message this snapshot restores to.
    pub message_id: Uuid,
    /// Tracking-path → backup.
    pub tracked_file_backups: BTreeMap<String, FileHistoryBackup>,
}

#[derive(Debug, Default)]
struct State {
    snapshots: Vec<FileHistorySnapshot>,
    tracked_files: HashSet<String>,
}

impl State {
    /// Resolve exactly one owned backup per tracked file while the caller holds
    /// the state lock. The returned plan never borrows mutable history across
    /// filesystem I/O and preserves the tracked set's existing iteration order.
    fn restore_plan(&self, message_id: Uuid) -> Option<Vec<(String, FileHistoryBackup)>> {
        let target = self
            .snapshots
            .iter()
            .rev()
            .find(|snapshot| snapshot.message_id == message_id)?;
        let mut missing: HashSet<&str> = self
            .tracked_files
            .iter()
            .filter(|path| !target.tracked_file_backups.contains_key(*path))
            .map(String::as_str)
            .collect();
        let mut first_versions = HashMap::new();
        for snapshot in &self.snapshots {
            if missing.is_empty() {
                break;
            }
            for (path, backup) in &snapshot.tracked_file_backups {
                if backup.version == 1 && missing.remove(path.as_str()) {
                    first_versions.insert(path.as_str(), backup);
                }
            }
        }
        Some(
            self.tracked_files
                .iter()
                .filter_map(|path| {
                    target
                        .tracked_file_backups
                        .get(path)
                        .or_else(|| first_versions.get(path.as_str()).copied())
                        .map(|backup| (path.clone(), backup.clone()))
                })
                .collect(),
        )
    }
}

/// A serialized `file-history-snapshot` side-map line (persisted to the session
/// JSONL; parsed back on resume).
#[derive(Debug, Clone)]
pub struct SnapshotRecord {
    /// The snapshot's message id.
    pub message_id: Uuid,
    /// Tracking-path → (backup_file_name-or-null, version).
    pub tracked_file_backups: BTreeMap<String, FileHistoryBackup>,
}

/// The session-scoped file-history store (behind an internal lock; cheap to
/// clone the `Arc`).
pub struct FileHistory {
    state: Mutex<State>,
    /// Config home (`~/.lingxi`) — the backup root parent.
    home: PathBuf,
    /// Bare session uuid — the backup dir segment.
    session_id: String,
}

impl FileHistory {
    /// New store for `session_id` under `home`. Tracked paths retain their
    /// original absolute location even when a session changes workspace.
    #[must_use]
    pub fn new(home: PathBuf, session_id: String) -> Self {
        Self {
            state: Mutex::new(State::default()),
            home,
            session_id,
        }
    }

    /// Rebuild the in-memory index from persisted `file-history-snapshot`
    /// records (resume). Every tracking path must name its original absolute
    /// source. A relative path has no reliable workspace provenance.
    pub fn restore_from_records(&self, records: Vec<SnapshotRecord>) -> Result<(), String> {
        let mut tracked = HashSet::new();
        let mut sources = HashMap::new();
        let mut snapshots = Vec::with_capacity(records.len());
        for rec in records {
            let mut backups = BTreeMap::new();
            for (path, backup) in rec.tracked_file_backups {
                validate_tracking_path(&path)?;
                validate_backup(&path, &backup)?;
                if let Some(previous) = sources.insert(path.clone(), backup.source.clone()) {
                    if previous != backup.source {
                        return Err(
                            "file-history checkpoint changes a tracked path's source authority"
                                .into(),
                        );
                    }
                }
                tracked.insert(path.clone());
                backups.insert(path, backup);
            }
            snapshots.push(FileHistorySnapshot {
                message_id: rec.message_id,
                tracked_file_backups: backups,
            });
        }
        let mut st = self.state.lock().expect("file-history lock");
        st.snapshots = snapshots;
        st.tracked_files = tracked;
        Ok(())
    }

    /// `true` when at least one snapshot restores to `message_id` (the picker's
    /// per-row `canRestore`).
    #[must_use]
    pub fn can_restore(&self, message_id: Uuid) -> bool {
        self.state
            .lock()
            .expect("file-history lock")
            .snapshots
            .iter()
            .any(|s| s.message_id == message_id)
    }

    /// Back up `file_path`'s CURRENT (pre-edit) content into the most-recent
    /// snapshot, if not already tracked there. MUST be called before the tool
    /// writes. No-op when no snapshot exists yet (no active turn).
    pub async fn track_edit(&self, file_path: &str) {
        // The editing tools supply their resolved absolute path. A relative
        // input cannot be interpreted against the session's original cwd: the
        // session may already have moved to another workspace.
        if validate_tracking_path(file_path).is_err() {
            return;
        }
        let tracking_path = file_path.to_owned();

        // Phase 1: is a backup needed? (already tracked in the most-recent
        // snapshot ⇒ skip; re-backing v1 would corrupt it with post-edit bytes.)
        let prior_source = {
            let st = self.state.lock().expect("file-history lock");
            let Some(recent) = st.snapshots.last() else {
                return;
            };
            if recent.tracked_file_backups.contains_key(&tracking_path) {
                return;
            }
            st.snapshots.iter().rev().find_map(|snapshot| {
                snapshot
                    .tracked_file_backups
                    .get(&tracking_path)
                    .map(|backup| backup.source.clone())
            })
        };

        // Phase 2: async backup (v1) outside the lock.
        let path = Path::new(file_path);
        let source = match prior_source {
            Some(source) => {
                if source.verify(path).is_err() {
                    return;
                }
                source
            }
            None => match FileHistorySource::capture(path) {
                Ok(source) => source,
                Err(_) => return,
            },
        };
        let Ok(backup) = self.create_backup(path, 1, &source).await else {
            return;
        };

        // Phase 3: commit — re-check under the lock (a racing track_edit may
        // have added it), then retroactively track it in the most-recent snap.
        let mut st = self.state.lock().expect("file-history lock");
        let Some(recent) = st.snapshots.last_mut() else {
            return;
        };
        if recent.tracked_file_backups.contains_key(&tracking_path) {
            return;
        }
        recent
            .tracked_file_backups
            .insert(tracking_path.clone(), backup);
        st.tracked_files.insert(tracking_path);
    }

    fn track_completed_write(&self, file_path: &str, parent_receipt: rooted_fs::RootedWriteParent) {
        if validate_tracking_path(file_path).is_err() {
            return;
        }
        let path = Path::new(file_path);
        let Some(parent) = path.parent() else { return };
        let Ok(parent_identity) = parent_receipt.identity() else {
            return;
        };
        let actual_parent = FileHistorySource {
            root: parent.to_path_buf(),
            root_identity: parent_identity,
        };
        if actual_parent.verify(path).is_err() {
            return;
        }
        let mut state = self.state.lock().expect("file-history lock");
        let Some((last, older)) = state.snapshots.split_last_mut() else {
            return;
        };
        // A pending checkpoint may only be upgraded before it is persisted;
        // never rewrite the meaning of a previously emitted snapshot.
        if older
            .iter()
            .any(|snapshot| snapshot.tracked_file_backups.contains_key(file_path))
        {
            return;
        }
        let Some(backup) = last.tracked_file_backups.get_mut(file_path) else {
            return;
        };
        if backup.source.has_original_parent(path)
            || backup.backup_file_name.is_some()
            || backup.source.verify(path).is_err()
            || !parent.starts_with(&backup.source.root)
            || actual_parent.verify(path).is_err()
        {
            return;
        }
        backup.source = actual_parent;
        drop(parent_receipt);
    }

    /// Add a new snapshot for `message_id`, backing up any tracked file that
    /// changed since its latest backup (once per user turn). Returns the record
    /// to persist (so the caller can append it to the session JSONL).
    pub async fn make_snapshot(&self, message_id: Uuid) -> SnapshotRecord {
        // Phase 1: capture the tracked set + latest backups.
        let (tracked, latest): (Vec<String>, HashMap<String, FileHistoryBackup>) = {
            let st = self.state.lock().expect("file-history lock");
            let latest = st
                .snapshots
                .last()
                .map(|s| {
                    s.tracked_file_backups
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                })
                .unwrap_or_default();
            (st.tracked_files.iter().cloned().collect(), latest)
        };

        // Phase 2: async IO — decide each tracked file's backup for this snap.
        let mut backups: BTreeMap<String, FileHistoryBackup> = BTreeMap::new();
        for tracking_path in tracked {
            let file_path = PathBuf::from(&tracking_path);
            let Some(latest_backup) = latest.get(&tracking_path) else {
                // Never infer a missing provenance from the current workspace.
                continue;
            };
            if !latest_backup.source.has_original_parent(&file_path) {
                // This file was first tracked before one or more parents were
                // created. Without a post-write directory receipt, a later
                // directory at that pathname cannot be identified as original.
                backups.insert(tracking_path, latest_backup.clone());
                continue;
            }
            let next_version = latest_backup.version.saturating_add(1);
            // File exists — reuse the latest backup if unchanged, else re-back.
            if latest_backup.backup_file_name.is_some()
                && !self.origin_file_changed(&file_path, latest_backup).await
            {
                backups.insert(tracking_path, latest_backup.clone());
                continue;
            }
            if let Ok(fresh) = self
                .create_backup(&file_path, next_version, &latest_backup.source)
                .await
            {
                backups.insert(tracking_path, fresh);
            }
        }

        // Phase 3: commit the new snapshot (inherit any file a racing track_edit
        // added), cap to MAX_SNAPSHOTS.
        let backup_names_to_delete = {
            let mut st = self.state.lock().expect("file-history lock");
            if let Some(last) = st.snapshots.last() {
                let inherit: Vec<(String, FileHistoryBackup)> = st
                    .tracked_files
                    .iter()
                    .filter(|p| !backups.contains_key(*p))
                    .filter_map(|p| {
                        last.tracked_file_backups
                            .get(p)
                            .map(|b| (p.clone(), b.clone()))
                    })
                    .collect();
                for (p, b) in inherit {
                    backups.insert(p, b);
                }
            }
            st.snapshots.push(FileHistorySnapshot {
                message_id,
                tracked_file_backups: backups.clone(),
            });
            let len = st.snapshots.len();
            if len > MAX_SNAPSHOTS {
                let removed: Vec<FileHistorySnapshot> =
                    st.snapshots.drain(0..len - MAX_SNAPSHOTS).collect();
                Self::orphaned_backup_names(&removed, &st.snapshots)
            } else {
                Vec::new()
            }
        };
        for backup_name in backup_names_to_delete {
            let _ = tokio::fs::remove_file(self.resolve_backup_path(&backup_name)).await;
        }

        SnapshotRecord {
            message_id,
            tracked_file_backups: backups,
        }
    }

    /// The current in-memory snapshot record for `message_id` (the most-recent
    /// matching snapshot), with the backups `track_edit` accumulated during the
    /// turn. `None` when no snapshot exists for the message. Used to persist the
    /// POPULATED snapshot at turn end — [`Self::make_snapshot`]'s return value is
    /// captured at turn start, before any edit, so it is always empty.
    #[must_use]
    pub fn snapshot_record(&self, message_id: Uuid) -> Option<SnapshotRecord> {
        self.state
            .lock()
            .expect("file-history lock")
            .snapshots
            .iter()
            .rev()
            .find(|s| s.message_id == message_id)
            .map(|s| SnapshotRecord {
                message_id: s.message_id,
                tracked_file_backups: s.tracked_file_backups.clone(),
            })
    }

    /// Restore the tracked files to the snapshot keyed by `message_id`: files
    /// present at that version are rewritten from their backup (only if they
    /// differ now), files absent at that version are deleted.
    ///
    /// Link safety (2.1.216): a tracked path holding a symlink, hard link, or
    /// other non-regular file is REFUSED rather than written or deleted through,
    /// and counted in [`RewindOutcome::skipped_links`]. Before this the restore
    /// went through `fs::copy`, which follows the destination link and writes
    /// outside the approved tree.
    ///
    /// # Errors
    /// - The target-not-found message when `message_id` has no snapshot.
    /// - `"No files could be restored …"` when EVERY file that differed from the
    ///   target version failed to restore — the 2.1.260 fix for reporting success
    ///   after restoring nothing, spelled `canRewind:false` in the oracle. A
    ///   rewind where nothing differed is still `Ok` with an empty `changed`.
    pub async fn rewind_files(&self, message_id: Uuid) -> Result<RewindOutcome, String> {
        self.rewind_files_guarded(message_id, || Ok(())).await
    }

    async fn rewind_files_guarded(
        &self,
        message_id: Uuid,
        mut verify_source: impl FnMut() -> Result<(), String>,
    ) -> Result<RewindOutcome, String> {
        verify_source()?;
        let plan = self
            .state
            .lock()
            .expect("file-history lock")
            .restore_plan(message_id)
            .ok_or_else(|| "The selected snapshot was not found".to_string())?;

        let mut outcome = RewindOutcome::default();
        for (tracking_path, backup) in plan {
            let file_path = PathBuf::from(&tracking_path);
            verify_source().map_err(|error| {
                format!("rewind source changed; workspace may be partially restored: {error}")
            })?;
            match self.rewind_one(&file_path, &backup).await {
                RewindStep::Unchanged => {}
                RewindStep::Changed(path) => outcome.changed.push(path),
                RewindStep::SkippedLink => outcome.skipped_links += 1,
                RewindStep::Failed => outcome.failed += 1,
            }
            verify_source().map_err(|error| {
                format!("rewind source changed; workspace may be partially restored: {error}")
            })?;
        }
        verify_source().map_err(|error| {
            format!("rewind source changed; workspace may be partially restored: {error}")
        })?;
        // 2.1.260: a rewind that could not restore ANY of the files that needed
        // restoring is not a success. The oracle spells this `canRewind:false`
        // and the CLI prints the error instead of "Files rewound to state at …".
        // A rewind where nothing differed has `changed == 0 && failed == 0` and
        // stays `Ok` — "nothing to do" is not a failure.
        if outcome.changed.is_empty() && outcome.failed > 0 {
            return Err(format!(
                "No files could be restored to the state at this message ({} file{} failed or \
                 have an uncertain result; the workspace may be partially restored)",
                outcome.failed,
                if outcome.failed == 1 { "" } else { "s" }
            ));
        }
        Ok(outcome)
    }

    /// Apply the target version to ONE tracked path.
    ///
    /// `backup_file_name` is `None` when the file did not exist at the target
    /// version (→ delete) and `Some(name)` when it did (→ restore from backup).
    async fn rewind_one(&self, file_path: &Path, backup: &FileHistoryBackup) -> RewindStep {
        if !backup.source.has_original_parent(file_path) {
            return RewindStep::SkippedLink;
        }
        let current = match backup.source.open(file_path) {
            Ok(current) => current,
            Err(_) => return RewindStep::SkippedLink,
        };
        match backup.backup_file_name.as_deref() {
            None => {
                // Absent at the target version → delete if present.
                if current.is_none() {
                    return RewindStep::Unchanged;
                }
                if backup.source.verify(file_path).is_err() {
                    return RewindStep::SkippedLink;
                }
                let relative = match backup.source.relative(file_path) {
                    Ok(relative) => relative,
                    Err(_) => return RewindStep::SkippedLink,
                };
                match rooted_fs::remove_file_pinned(
                    &backup.source.root,
                    relative,
                    &backup.source.root_identity,
                ) {
                    Ok(()) => RewindStep::Changed(file_path.to_string_lossy().into_owned()),
                    Err(FsError::NotFound(_)) => RewindStep::Unchanged,
                    Err(FsError::OutsideWorkspace(_)) => RewindStep::SkippedLink,
                    Err(_) => RewindStep::Failed,
                }
            }
            Some(name) => {
                if !self.origin_file_changed(file_path, backup).await {
                    return RewindStep::Unchanged;
                }
                match self.restore_backup(file_path, name, &backup.source).await {
                    Ok(true) => RewindStep::Changed(file_path.to_string_lossy().into_owned()),
                    // The backup file itself is gone. Before this, `restore_backup`
                    // returned `Ok(())` here and the path was still pushed onto
                    // `changed` — the file was reported as restored having never
                    // been touched.
                    Err(FsError::OutsideWorkspace(_)) => RewindStep::SkippedLink,
                    Ok(false) | Err(_) => RewindStep::Failed,
                }
            }
        }
    }

    /// Would rewinding to `message_id` change any file on disk? (the picker's
    /// per-row `has_code_changes` flag — early-exits, never diffs).
    #[must_use]
    pub async fn has_any_changes(&self, message_id: Uuid) -> bool {
        let plan = self
            .state
            .lock()
            .expect("file-history lock")
            .restore_plan(message_id);
        let Some(plan) = plan else {
            return false;
        };
        for (tracking_path, backup) in plan {
            let file_path = PathBuf::from(&tracking_path);
            if !backup.source.has_original_parent(&file_path) {
                return true;
            }
            match backup.backup_file_name.as_deref() {
                None => {
                    if !matches!(backup.source.open(&file_path), Ok(None)) {
                        return true;
                    }
                }
                Some(_) => {
                    if self.origin_file_changed(&file_path, &backup).await {
                        return true;
                    }
                }
            }
        }
        false
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    fn orphaned_backup_names(
        removed: &[FileHistorySnapshot],
        retained: &[FileHistorySnapshot],
    ) -> Vec<String> {
        let retained_names: HashSet<&str> = retained
            .iter()
            .flat_map(|snap| snap.tracked_file_backups.values())
            .filter_map(|backup| backup.backup_file_name.as_deref())
            .collect();
        let mut names = HashSet::new();
        for snap in removed {
            for backup in snap.tracked_file_backups.values() {
                if let Some(name) = backup.backup_file_name.as_deref() {
                    if !retained_names.contains(name) {
                        names.insert(name.to_string());
                    }
                }
            }
        }
        names.into_iter().collect()
    }

    /// `<hash>@v<n>` backup file name (sha256 of the path, first 16 hex chars).
    fn backup_file_name(file_path: &str, version: u32) -> String {
        let mut hasher = Sha256::new();
        hasher.update(file_path.as_bytes());
        let hash = hasher.finalize();
        let hex: String = hash.iter().take(8).map(|b| format!("{b:02x}")).collect();
        format!("{hex}@v{version}")
    }

    /// Absolute path of a backup file under `<home>/file-history/<session>/`.
    fn resolve_backup_path(&self, backup_file_name: &str) -> PathBuf {
        self.home
            .join("file-history")
            .join(&self.session_id)
            .join(backup_file_name)
    }

    /// Copy the originally-authorized file through its pinned parent. A missing
    /// source records a null backup; an invalid parent never becomes a null.
    async fn create_backup(
        &self,
        file_path: &Path,
        version: u32,
        source: &FileHistorySource,
    ) -> Result<FileHistoryBackup, FsError> {
        let path = file_path.to_path_buf();
        let source = source.clone();
        let home = self.home.clone();
        let session_id = self.session_id.clone();
        tokio::task::spawn_blocking(move || {
            let Some(mut original) = source.open(&path)? else {
                return Ok(FileHistoryBackup {
                    backup_file_name: None,
                    version,
                    source,
                });
            };
            let mode = file_mode(&original);
            let name = Self::backup_file_name(&path.to_string_lossy(), version);
            let home =
                std::fs::canonicalize(home).map_err(|error| FsError::Io(error.to_string()))?;
            let identity = rooted_fs::root_identity(&home)?;
            let relative = Path::new("file-history").join(session_id).join(&name);
            rooted_fs::atomic_write_stream_pinned_checked(
                &home,
                &relative,
                rooted_fs::AtomicWriteOptions {
                    overwrite: true,
                    create_parents: true,
                    dir_mode: 0o700,
                    file_mode: mode,
                },
                &identity,
                |file| {
                    std::io::copy(&mut original, file)
                        .map(|_| ())
                        .map_err(|error| FsError::Io(error.to_string()))
                },
                || source.verify(&path),
            )
            .map_err(|error| FsError::Io(error.to_string()))?;
            Ok(FileHistoryBackup {
                backup_file_name: Some(name),
                version,
                source,
            })
        })
        .await
        .map_err(|error| FsError::Io(error.to_string()))?
    }

    /// Overwrite `file_path` from its backup (lazy-mkdir on ENOENT).
    ///
    /// Returns `Ok(false)` when the backup file is MISSING — the caller must
    /// treat that as a failure to restore, not as success. Returning `Ok(())`
    /// here is what let `/rewind` report a file as restored after doing nothing
    /// (2.1.260).
    async fn restore_backup(
        &self,
        file_path: &Path,
        backup_file_name: &str,
        source: &FileHistorySource,
    ) -> Result<bool, FsError> {
        let Some(mut backup) = self.open_backup(backup_file_name)? else {
            return Ok(false);
        };
        let mode = file_mode(&backup);
        let path = file_path.to_path_buf();
        let source = source.clone();
        tokio::task::spawn_blocking(move || {
            source.verify(&path)?;
            let relative = source.relative(&path)?;
            rooted_fs::atomic_write_stream_pinned_checked(
                &source.root,
                relative,
                rooted_fs::AtomicWriteOptions {
                    overwrite: true,
                    create_parents: true,
                    dir_mode: 0o777,
                    file_mode: mode,
                },
                &source.root_identity,
                |file| {
                    std::io::copy(&mut backup, file)
                        .map(|_| ())
                        .map_err(|error| FsError::Io(error.to_string()))
                },
                || source.verify(&path),
            )
            .map_err(|error| FsError::Io(error.to_string()))?;
            source.verify(&path)?;
            Ok(true)
        })
        .await
        .map_err(|error| FsError::Io(error.to_string()))?
    }

    fn open_backup(&self, name: &str) -> Result<Option<std::fs::File>, FsError> {
        let home =
            std::fs::canonicalize(&self.home).map_err(|error| FsError::Io(error.to_string()))?;
        let identity = rooted_fs::root_identity(&home)?;
        let relative = Path::new("file-history").join(&self.session_id).join(name);
        let file = match rooted_fs::open_read_file_pinned(&home, &relative, Some(&identity)) {
            Ok(file) => file,
            Err(FsError::NotFound(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        rooted_fs::validate_open_file_single_link(&file)?;
        Ok(Some(file))
    }

    /// Compare through the originally captured parent, never the current cwd.
    async fn origin_file_changed(&self, original: &Path, backup: &FileHistoryBackup) -> bool {
        let Some(name) = backup.backup_file_name.as_deref() else {
            return true;
        };
        let Ok(Some(orig_file)) = backup.source.open(original) else {
            return true;
        };
        let Ok(Some(back_file)) = self.open_backup(name) else {
            return true;
        };
        let changed = match (orig_file.metadata(), back_file.metadata()) {
            (Ok(a), Ok(b)) if a.len() != b.len() => true,
            (Ok(_), Ok(_)) => files_differ(orig_file, back_file).await,
            _ => true,
        };
        changed || backup.source.verify(original).is_err()
    }
}

fn validate_tracking_path(path: &str) -> Result<(), String> {
    use std::path::Component;
    let path = Path::new(path);
    if !path.is_absolute()
        || !path
            .components()
            .any(|part| matches!(part, Component::Normal(_)))
        || path.components().any(|part| {
            !matches!(
                part,
                Component::Prefix(_) | Component::RootDir | Component::Normal(_)
            )
        })
    {
        return Err(format!(
            "file-history checkpoint has no valid absolute source path: {}",
            path.display()
        ));
    }
    Ok(())
}

fn validate_backup(path: &str, backup: &FileHistoryBackup) -> Result<(), String> {
    use std::path::Component;
    if backup.version == 0 {
        return Err("file-history checkpoint has an invalid backup version".into());
    }
    if !backup.source.root.is_absolute()
        || backup.source.relative(Path::new(path)).is_err()
        || backup.source.root.components().any(|part| {
            !matches!(
                part,
                Component::Prefix(_) | Component::RootDir | Component::Normal(_)
            )
        })
    {
        return Err("file-history checkpoint has an invalid source authority".into());
    }
    if let Some(name) = backup.backup_file_name.as_deref() {
        let mut parts = Path::new(name).components();
        if !matches!(parts.next(), Some(Component::Normal(_))) || parts.next().is_some() {
            return Err("file-history checkpoint has an invalid backup name".into());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn file_mode(file: &std::fs::File) -> u32 {
    use std::os::unix::fs::MetadataExt;
    file.metadata()
        .map(|metadata| metadata.mode() & 0o777)
        .unwrap_or(0o600)
}

#[cfg(not(unix))]
fn file_mode(_file: &std::fs::File) -> u32 {
    0o600
}

async fn files_differ(original: std::fs::File, backup: std::fs::File) -> bool {
    use tokio::io::AsyncReadExt;
    let mut orig = tokio::fs::File::from_std(original);
    let mut back = tokio::fs::File::from_std(backup);
    // Keep the async state small. Fixed arrays become part of the nested
    // `has_any_changes` future and can overflow Tokio's test/runtime stack in
    // debug builds before the first read is polled.
    let mut orig_buf = vec![0u8; 65_536];
    let mut back_buf = vec![0u8; 65_536];
    loop {
        let orig_n = match orig.read(&mut orig_buf).await {
            Ok(n) => n,
            Err(_) => return true,
        };
        let back_n = match back.read(&mut back_buf).await {
            Ok(n) => n,
            Err(_) => return true,
        };
        if orig_n != back_n || orig_buf[..orig_n] != back_buf[..back_n] {
            return true;
        }
        if orig_n == 0 {
            return false;
        }
    }
}

/// Serialize a [`SnapshotRecord`] as a `file-history-snapshot` JSONL side-map
/// line (persisted to the session transcript so `/rewind` survives unwind /
/// `--resume`). Shape mirrors claude-code `recordFileHistorySnapshot`.
#[must_use]
pub fn snapshot_line_json(session_id: &str, record: &SnapshotRecord) -> serde_json::Value {
    let backups: serde_json::Map<String, serde_json::Value> = record
        .tracked_file_backups
        .iter()
        .map(|(path, b)| {
            (
                path.clone(),
                serde_json::json!({
                    "backupFileName": b.backup_file_name,
                    "version": b.version,
                    "source": b.source,
                }),
            )
        })
        .collect();
    serde_json::json!({
        "type": "file-history-snapshot",
        "sessionId": session_id,
        "messageId": record.message_id.to_string(),
        "trackedFileBackups": backups,
    })
}

/// Parse persisted `file-history-snapshot` rows, rejecting records without an
/// unambiguous absolute source path before any file-history operation.
pub fn parse_snapshot_records(content: &str) -> Result<Vec<SnapshotRecord>, String> {
    let mut out = Vec::new();
    // Snapshot rows are a small fraction of a long transcript: parse only the
    // lines that can be one.
    for line in crate::jsonl::reader::lines_naming_types(content, &["file-history-snapshot"]) {
        let v = serde_json::from_str::<serde_json::Value>(line)
            .map_err(|error| format!("invalid file-history checkpoint: {error}"))?;
        if v.get("type").and_then(serde_json::Value::as_str) != Some("file-history-snapshot") {
            continue;
        }
        let message_id = v
            .get("messageId")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or("file-history checkpoint has an invalid message ID")?;
        let mut backups = BTreeMap::new();
        let obj = v
            .get("trackedFileBackups")
            .and_then(serde_json::Value::as_object)
            .ok_or("file-history checkpoint has invalid tracked files")?;
        for (path, b) in obj {
            validate_tracking_path(path)?;
            let version = b
                .get("version")
                .and_then(serde_json::Value::as_u64)
                .and_then(|version| u32::try_from(version).ok())
                .ok_or("file-history checkpoint has an invalid backup version")?;
            let backup_file_name = match b.get("backupFileName") {
                Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(name)) => Some(name.clone()),
                _ => return Err("file-history checkpoint has an invalid backup name".into()),
            };
            let source = serde_json::from_value(
                b.get("source")
                    .cloned()
                    .ok_or("file-history checkpoint has no source authority")?,
            )
            .map_err(|_| "file-history checkpoint has an invalid source authority")?;
            let backup = FileHistoryBackup {
                backup_file_name,
                version,
                source,
            };
            validate_backup(path, &backup)?;
            backups.insert(path.clone(), backup);
        }
        out.push(SnapshotRecord {
            message_id,
            tracked_file_backups: backups,
        });
    }
    Ok(out)
}

/// The message ids that have a persisted checkpoint (the `/rewind` picker's
/// candidate rows), in transcript order.
pub fn checkpoint_message_ids(content: &str) -> Result<Vec<Uuid>, String> {
    Ok(parse_snapshot_records(content)?
        .into_iter()
        .map(|r| r.message_id)
        .collect())
}

/// Restore working-tree checkpoints from the claimed, captured transcript.
/// The task retains the session claim even if its caller stops waiting.
pub async fn rewind_from_captured(
    home: &Path,
    cwd: &str,
    session_id: Uuid,
    mode: lingxi_core::host::session_context::ConversationMode,
    authority: &crate::jsonl::writer::TranscriptFileAuthority,
    content: &str,
    message_id: Uuid,
    lease: lingxi_core::host::live_sessions::SharedSessionWriterLease,
) -> Result<RewindOutcome, String> {
    if lease.canonical_session_id().map(|id| id.as_uuid()) != Some(session_id) {
        return Err("rewind session writer claim does not match".into());
    }
    if std::fs::canonicalize(home).map_err(|error| error.to_string())? != authority.root {
        return Err("rewind transcript is outside the claimed session home".into());
    }
    let home = home.to_path_buf();
    let cwd = PathBuf::from(cwd);
    let authority = authority.clone();
    let content = content.to_owned();
    tokio::spawn(async move {
        let _lease = lease;
        crate::rewind::validate_captured_source(&authority, &content, session_id, mode, &cwd)?;
        let revision = crate::rewind::source_revision(&content);
        let records = parse_snapshot_records(&content)?;
        let fh = FileHistory::new(home, session_id.to_string());
        fh.restore_from_records(records)?;
        let outcome = fh.rewind_files_guarded(message_id, || {
            authority
                .verify_revision(&revision)
                .map_err(|error| error.to_string())
        })
        .await?;
        if outcome.skipped_links != 0 || outcome.failed != 0 {
            return Err(format!(
                "file rewind refused an unavailable or changed source; workspace may be partially restored ({} skipped, {} failed)",
                outcome.skipped_links, outcome.failed,
            ));
        }
        Ok(outcome)
    })
    .await
    .map_err(|error| {
        format!("file rewind task failed; workspace may be partially restored: {error}")
    })?
}

#[async_trait::async_trait]
impl lingxi_core::host::FileHistorySink for FileHistory {
    async fn track_edit(&self, file_path: &str) {
        FileHistory::track_edit(self, file_path).await;
    }

    fn track_completed_write(&self, file_path: &str, parent: rooted_fs::RootedWriteParent) {
        FileHistory::track_completed_write(self, file_path, parent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> (PathBuf, PathBuf) {
        let base =
            std::env::temp_dir().join(format!("lingxi-filehist-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        let cwd = base.join("proj");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        (home.canonicalize().unwrap(), cwd.canonicalize().unwrap())
    }

    #[test]
    fn restore_plan_keeps_latest_target_first_v1_authority_and_tracked_order() {
        let (_home, cwd) = scratch("plan-semantics");
        let source = FileHistorySource::capture(&cwd.join("target")).unwrap();
        let names = ["target", "fallback", "delete", "unresolved"]
            .map(|name| cwd.join(name).display().to_string());
        let backup = |name: Option<&str>, version| FileHistoryBackup {
            backup_file_name: name.map(str::to_owned),
            version,
            source: source.clone(),
        };
        let message_id = Uuid::new_v4();
        let mut state = State {
            tracked_files: names.iter().cloned().collect(),
            snapshots: vec![
                FileHistorySnapshot {
                    message_id: Uuid::new_v4(),
                    tracked_file_backups: BTreeMap::from([
                        (names[1].clone(), backup(Some("first@v1"), 1)),
                        (names[2].clone(), backup(None, 1)),
                    ]),
                },
                FileHistorySnapshot {
                    message_id,
                    tracked_file_backups: BTreeMap::from([(
                        names[0].clone(),
                        backup(Some("old-target@v2"), 2),
                    )]),
                },
                FileHistorySnapshot {
                    message_id,
                    tracked_file_backups: BTreeMap::from([(
                        names[0].clone(),
                        backup(Some("latest-target@v3"), 3),
                    )]),
                },
                FileHistorySnapshot {
                    message_id: Uuid::new_v4(),
                    tracked_file_backups: BTreeMap::from([(
                        names[1].clone(),
                        backup(Some("later@v1"), 1),
                    )]),
                },
            ],
        };
        let order: Vec<_> = state
            .tracked_files
            .iter()
            .filter(|path| *path != &names[3])
            .cloned()
            .collect();
        BACKUP_CLONE_COUNT.with(|count| count.set(0));
        let plan = state.restore_plan(message_id).unwrap();
        assert_eq!(BACKUP_CLONE_COUNT.with(std::cell::Cell::get), 3);
        assert_eq!(
            plan.iter()
                .map(|(path, _)| path.clone())
                .collect::<Vec<_>>(),
            order
        );
        let selected: BTreeMap<_, _> = plan.into_iter().collect();
        assert_eq!(
            selected[&names[0]].backup_file_name.as_deref(),
            Some("latest-target@v3")
        );
        assert_eq!(
            selected[&names[1]].backup_file_name.as_deref(),
            Some("first@v1")
        );
        assert_eq!(selected[&names[1]].source, source);
        assert_eq!(selected[&names[2]].backup_file_name, None);
        assert!(!selected.contains_key(&names[3]));
        state.snapshots.clear();
        assert_eq!(
            selected[&names[0]].version, 3,
            "the plan owns a coherent selection"
        );
    }

    #[test]
    fn restore_plan_backup_copies_scale_with_files_instead_of_snapshot_count() {
        let (_home, cwd) = scratch("plan-scaling");
        let source = FileHistorySource::capture(&cwd.join("tracked")).unwrap();
        let paths: Vec<_> = (0..32)
            .map(|i| cwd.join(format!("file-{i}")).display().to_string())
            .collect();
        for snapshot_count in [2, 100, 1_000] {
            let message_id = Uuid::new_v4();
            let snapshots = (0..snapshot_count)
                .map(|index| FileHistorySnapshot {
                    message_id: if index + 1 == snapshot_count {
                        message_id
                    } else {
                        Uuid::new_v4()
                    },
                    tracked_file_backups: paths
                        .iter()
                        .enumerate()
                        // Half the files require a first-version fallback at the target.
                        .filter(|(i, _)| index + 1 != snapshot_count || i % 2 == 0)
                        .map(|(_, path)| {
                            (
                                path.clone(),
                                FileHistoryBackup {
                                    backup_file_name: Some(format!("backup@v{}", index + 1)),
                                    version: index + 1,
                                    source: source.clone(),
                                },
                            )
                        })
                        .collect(),
                })
                .collect();
            let state = State {
                snapshots,
                tracked_files: paths.iter().cloned().collect(),
            };
            BACKUP_CLONE_COUNT.with(|count| count.set(0));
            let plan = state.restore_plan(message_id).unwrap();
            assert_eq!(plan.len(), paths.len());
            assert_eq!(BACKUP_CLONE_COUNT.with(std::cell::Cell::get), paths.len());
            BACKUP_CLONE_COUNT.with(|count| count.set(0));
            assert!(state.restore_plan(Uuid::new_v4()).is_none());
            assert_eq!(BACKUP_CLONE_COUNT.with(std::cell::Cell::get), 0);
        }
    }

    #[tokio::test]
    async fn rewind_plan_uses_v1_fallback_without_holding_history_lock_during_io() {
        let (home, cwd) = scratch("plan-rewind");
        let existing = cwd.join("existing.txt");
        let created = cwd.join("created.txt");
        std::fs::write(&existing, "original").unwrap();
        let history = FileHistory::new(home, "plan-rewind".into());
        let target = Uuid::new_v4();
        history.make_snapshot(target).await;
        history.make_snapshot(Uuid::new_v4()).await;
        history.track_edit(existing.to_str().unwrap()).await;
        history.track_edit(created.to_str().unwrap()).await;
        std::fs::write(&existing, "changed").unwrap();
        std::fs::write(&created, "new").unwrap();
        assert!(history.has_any_changes(target).await);
        let outcome = history
            .rewind_files_guarded(target, || {
                let _state = history
                    .state
                    .try_lock()
                    .expect("plan lock is released before filesystem work");
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(outcome.changed.len(), 2);
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "original");
        assert!(!created.exists());
        assert!(!history.has_any_changes(target).await);
    }

    #[test]
    fn restored_history_retains_snapshots_beyond_live_pruning_limit() {
        let (home, cwd) = scratch("plan-resume-retention");
        let path = cwd.join("tracked").display().to_string();
        let source = FileHistorySource::capture(Path::new(&path)).unwrap();
        let first = Uuid::new_v4();
        let history = FileHistory::new(home, "plan-resume-retention".into());
        history
            .restore_from_records(
                (0..MAX_SNAPSHOTS + 10)
                    .map(|index| SnapshotRecord {
                        message_id: if index == 0 { first } else { Uuid::new_v4() },
                        tracked_file_backups: BTreeMap::from([(
                            path.clone(),
                            FileHistoryBackup {
                                backup_file_name: None,
                                version: u32::try_from(index + 1).unwrap(),
                                source: source.clone(),
                            },
                        )]),
                    })
                    .collect(),
            )
            .unwrap();
        let state = history.state.lock().unwrap();
        assert_eq!(state.snapshots.len(), MAX_SNAPSHOTS + 10);
        assert_eq!(state.restore_plan(first).unwrap()[0].1.version, 1);
    }

    #[tokio::test]
    async fn edit_then_rewind_restores_pre_edit_content() {
        let (home, cwd) = scratch("restore");
        let file = cwd.join("a.txt");
        std::fs::write(&file, "v0\n").unwrap();
        let fh = FileHistory::new(home, "sess1".into());

        let msg = Uuid::new_v4();
        // Turn opens: snapshot, then the tool backs up pre-edit content, then
        // the tool writes the new content.
        fh.make_snapshot(msg).await;
        fh.track_edit(file.to_str().unwrap()).await;
        std::fs::write(&file, "v1-edited\n").unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1-edited\n");

        // Rewind to the turn → the pre-edit content is restored.
        let changed = fh.rewind_files(msg).await.expect("rewind").changed;
        assert_eq!(changed.len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v0\n");
    }

    #[tokio::test]
    async fn captured_file_rewind_stops_on_replaced_source_and_reports_partial_work() {
        for swap_at in [2, 3] {
            let (home, cwd) = scratch(&format!("source-swap-{swap_at}"));
            let first = cwd.join("first.txt");
            let second = cwd.join("second.txt");
            std::fs::write(&first, "before").unwrap();
            std::fs::write(&second, "before").unwrap();
            let fh = FileHistory::new(home.clone(), "session".into());
            let message = Uuid::new_v4();
            fh.make_snapshot(message).await;
            fh.track_edit(first.to_str().unwrap()).await;
            fh.track_edit(second.to_str().unwrap()).await;
            std::fs::write(&first, "after").unwrap();
            std::fs::write(&second, "after").unwrap();
            let home = home.canonicalize().unwrap();
            let relative = PathBuf::from("projects/bucket/source.jsonl");
            let path = home.join(&relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "source\n").unwrap();
            let (authority, _) =
                crate::jsonl::writer::TranscriptFileAuthority::capture(home.clone(), relative)
                    .unwrap();
            let revision = crate::rewind::source_revision("source\n");
            let retired = home.join("retired.jsonl");
            let mut checks = 0;
            let error = fh
                .rewind_files_guarded(message, || {
                    checks += 1;
                    if checks == swap_at {
                        std::fs::rename(&path, &retired).unwrap();
                        std::fs::write(&path, "source\n").unwrap();
                    }
                    authority
                        .verify_revision(&revision)
                        .map_err(|error| error.to_string())
                })
                .await
                .unwrap_err();
            assert!(
                error.contains("workspace may be partially restored"),
                "{error}"
            );
            let restored = [first, second]
                .iter()
                .filter(|path| std::fs::read_to_string(path).unwrap() == "before")
                .count();
            assert_eq!(restored, usize::from(swap_at == 3));
            assert_eq!(std::fs::read_to_string(path).unwrap(), "source\n");
            assert_eq!(std::fs::read_to_string(retired).unwrap(), "source\n");
        }
    }

    /// 2.1.260 — the regression this file shipped with: `restore_backup`
    /// returned `Ok(())` when the checkpoint backup was missing and the caller
    /// pushed the path onto `changed` anyway, so `/rewind` printed "Files
    /// rewound to state at message …" having touched nothing.
    ///
    /// The oracle's rule is narrow: per-file failures are not `skippedLinks`,
    /// but when EVERY differing file fails the rewind itself fails
    /// (`canRewind:false`).
    #[tokio::test]
    async fn a_missing_backup_fails_the_rewind_instead_of_reporting_success() {
        let (home, cwd) = scratch("missing-backup");
        let file = cwd.join("a.txt");
        std::fs::write(&file, "v0\n").unwrap();
        let fh = FileHistory::new(home.clone(), "sess-mb".into());

        let msg = Uuid::new_v4();
        fh.make_snapshot(msg).await;
        fh.track_edit(file.to_str().unwrap()).await;
        std::fs::write(&file, "v1-edited\n").unwrap();

        // Delete the checkpoint backup out from under the rewind.
        let backup_dir = home.join("file-history").join("sess-mb");
        let mut removed = 0usize;
        for entry in std::fs::read_dir(&backup_dir).expect("backup dir exists") {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
            removed += 1;
        }
        assert_eq!(removed, 1, "the test must actually remove the one backup");

        let err = fh
            .rewind_files(msg)
            .await
            .expect_err("a rewind that restored nothing is not a success");
        assert!(
            err.contains("No files could be restored"),
            "error must name what happened, got: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "v1-edited\n",
            "the edited content is still on disk — nothing was restored"
        );
    }

    /// 2.1.216 — `fs::copy` writes THROUGH a destination symlink, so a tracked
    /// path swapped for a link to somewhere else let `/rewind` overwrite a file
    /// outside the checkpoint. The link-safety check refuses and counts it.
    #[cfg(unix)]
    #[tokio::test]
    async fn rewind_refuses_to_write_through_a_symlinked_tracked_path() {
        let (home, cwd) = scratch("symlink");
        let file = cwd.join("a.txt");
        std::fs::write(&file, "v0\n").unwrap();
        let fh = FileHistory::new(home, "sess-sym".into());

        let msg = Uuid::new_v4();
        fh.make_snapshot(msg).await;
        fh.track_edit(file.to_str().unwrap()).await;
        std::fs::write(&file, "v1-edited\n").unwrap();

        // Swap the tracked path for a symlink pointing OUTSIDE the project.
        let outsider = cwd.parent().unwrap().join("outside-the-tree.txt");
        std::fs::write(&outsider, "DO NOT TOUCH\n").unwrap();
        std::fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(&outsider, &file).unwrap();

        let outcome = fh.rewind_files(msg).await.expect("rewind");
        assert_eq!(outcome.skipped_links, 1, "the link must be counted");
        assert!(
            outcome.changed.is_empty(),
            "nothing may be reported changed"
        );
        assert_eq!(
            std::fs::read_to_string(&outsider).unwrap(),
            "DO NOT TOUCH\n",
            "the symlink target outside the tree must be untouched"
        );
    }

    /// The delete arm escapes too: `remove_file` on a symlinked tracked path
    /// unlinks the LINK, leaving the real file — and reports it as rewound.
    #[cfg(unix)]
    #[tokio::test]
    async fn rewind_refuses_to_delete_through_a_symlinked_tracked_path() {
        let (home, cwd) = scratch("symlink-delete");
        let fh = FileHistory::new(home, "sess-symdel".into());
        let msg = Uuid::new_v4();
        fh.make_snapshot(msg).await;
        // Brand-new file at the tracked path → null backup → the rewind deletes.
        let file = cwd.join("new.txt");
        fh.track_edit(file.to_str().unwrap()).await;

        let outsider = cwd.parent().unwrap().join("delete-target.txt");
        std::fs::write(&outsider, "KEEP\n").unwrap();
        std::os::unix::fs::symlink(&outsider, &file).unwrap();

        let outcome = fh.rewind_files(msg).await.expect("rewind");
        assert_eq!(outcome.skipped_links, 1);
        assert!(outcome.changed.is_empty());
        assert!(outsider.exists(), "the link target must survive");
        assert!(
            std::fs::symlink_metadata(&file).is_ok(),
            "the link itself is left alone, not unlinked"
        );
    }

    /// A rewind where nothing differs is NOT a failure — `changed` is empty and
    /// `failed` is zero, so the all-failed arm must not fire. Without this the
    /// missing-backup fix would turn every no-op rewind into an error.
    #[tokio::test]
    async fn a_rewind_with_nothing_to_do_still_succeeds() {
        let (home, cwd) = scratch("noop");
        let file = cwd.join("a.txt");
        std::fs::write(&file, "v0\n").unwrap();
        let fh = FileHistory::new(home, "sess-noop".into());
        let msg = Uuid::new_v4();
        fh.make_snapshot(msg).await;
        fh.track_edit(file.to_str().unwrap()).await;
        // No edit after the backup — the file already matches the target.

        let outcome = fh.rewind_files(msg).await.expect("a no-op rewind is fine");
        assert!(outcome.changed.is_empty());
        assert_eq!(outcome.failed, 0);
        assert_eq!(outcome.skipped_links, 0);
    }

    #[tokio::test]
    async fn rewind_deletes_a_file_created_during_the_turn() {
        let (home, cwd) = scratch("delete");
        let fh = FileHistory::new(home, "sess2".into());
        let msg = Uuid::new_v4();
        fh.make_snapshot(msg).await;
        // A brand-new file: track_edit records a null backup (did not exist),
        // then the tool creates it.
        let file = cwd.join("new.txt");
        fh.track_edit(file.to_str().unwrap()).await;
        std::fs::write(&file, "created\n").unwrap();
        assert!(file.exists());

        let changed = fh.rewind_files(msg).await.expect("rewind").changed;
        assert_eq!(changed.len(), 1);
        assert!(!file.exists(), "file created after the snapshot is deleted");
    }

    /// The disk round-trip the orchestrator relies on for `/rewind`: the record
    /// that must be PERSISTED is the one `track_edit` populated during the turn
    /// (`snapshot_record`), NOT `make_snapshot`'s turn-start return (always empty
    /// — no edits have happened yet). Persisting the empty one — the original bug
    /// — left `rewind_from_disk` with nothing to restore. Reloading the populated
    /// record into a FRESH history (== `rewind_from_disk`) must still delete a
    /// file created during the turn.
    #[tokio::test]
    async fn disk_roundtrip_restores_from_populated_snapshot_record() {
        let (home, cwd) = scratch("roundtrip");
        let file = cwd.join("note.txt");
        let fh = FileHistory::new(home.clone(), "sess-rt".into());
        let msg = Uuid::new_v4();

        // Turn start: make_snapshot's return is EMPTY (the value that must NOT be
        // persisted).
        let turn_start = fh.make_snapshot(msg).await;
        assert!(
            turn_start.tracked_file_backups.is_empty(),
            "make_snapshot at turn start carries no backups"
        );

        // During the turn: a brand-new file is created (track_edit records the
        // absent pre-edit state, then the tool writes it).
        fh.track_edit(file.to_str().unwrap()).await;
        std::fs::write(&file, "created\n").unwrap();

        // Turn end: the POPULATED record carries the backup.
        let turn_end = fh.snapshot_record(msg).expect("snapshot exists");
        assert!(
            !turn_end.tracked_file_backups.is_empty(),
            "snapshot_record carries track_edit's accumulated backups"
        );

        // Persist the populated record and rebuild a FRESH history from it — the
        // exact path `rewind_from_disk` takes (parse_snapshot_records →
        // restore_from_records → rewind_files).
        let line = snapshot_line_json("sess-rt", &turn_end);
        let content = format!("{}\n", serde_json::to_string(&line).unwrap());
        let fresh = FileHistory::new(home, "sess-rt".into());
        fresh
            .restore_from_records(parse_snapshot_records(&content).unwrap())
            .unwrap();

        let changed = fresh.rewind_files(msg).await.expect("rewind").changed;
        assert_eq!(changed.len(), 1, "the created file is rewound (deleted)");
        assert!(
            !file.exists(),
            "disk-rebuilt rewind deletes the file created during the turn"
        );
    }

    #[tokio::test]
    async fn resumed_history_continues_tracking_and_versioning() {
        let (home, cwd) = scratch("resume-version");
        let file = cwd.join("note.txt");
        std::fs::write(&file, "before\n").unwrap();
        let first = FileHistory::new(home.clone(), "sess-resume".into());
        let first_message = Uuid::new_v4();
        first.make_snapshot(first_message).await;
        first.track_edit(file.to_str().unwrap()).await;
        std::fs::write(&file, "after first turn\n").unwrap();
        let first_record = first.snapshot_record(first_message).unwrap();
        let content = format!(
            "{}\n",
            serde_json::to_string(&snapshot_line_json("sess-resume", &first_record)).unwrap()
        );

        let resumed = FileHistory::new(home, "sess-resume".into());
        resumed
            .restore_from_records(parse_snapshot_records(&content).unwrap())
            .unwrap();
        let second_message = Uuid::new_v4();
        let second_record = resumed.make_snapshot(second_message).await;
        let backup = second_record
            .tracked_file_backups
            .get(file.to_str().unwrap())
            .expect("restored tracked file must carry into the next turn");
        assert_eq!(
            backup.version, 2,
            "resume must continue the per-file backup sequence"
        );
        assert!(
            backup.backup_file_name.is_some(),
            "the current file contents must be backed for the resumed turn"
        );
    }

    #[tokio::test]
    async fn cross_workspace_history_keeps_original_paths_and_business_files() {
        for had_backup in [false, true] {
            let (home, a) = scratch(&format!("cross-cwd-{had_backup}"));
            let b = a.parent().unwrap().join("other");
            std::fs::create_dir(&b).unwrap();
            let a_file = a.join("foo");
            let b_file = b.join("foo");
            if had_backup {
                std::fs::write(&a_file, b"A before").unwrap();
            }
            std::fs::write(&b_file, b"B business data").unwrap();
            let original = FileHistory::new(home.clone(), "cross-cwd".into());
            let first = Uuid::new_v4();
            original.make_snapshot(first).await;
            original.track_edit(a_file.to_str().unwrap()).await;
            std::fs::write(&a_file, b"A edited").unwrap();
            let checkpoint = original.snapshot_record(first).unwrap();
            let content = format!("{}\n", snapshot_line_json("cross-cwd", &checkpoint));

            // Hydration after /cd B must preserve A's authority for the next
            // snapshot as well as for the eventual rewind.
            let resumed = FileHistory::new(home, "cross-cwd".into());
            resumed
                .restore_from_records(parse_snapshot_records(&content).unwrap())
                .unwrap();
            let later = resumed.make_snapshot(Uuid::new_v4()).await;
            let stored = later
                .tracked_file_backups
                .get(a_file.to_str().unwrap())
                .unwrap();
            assert_eq!(
                stored.source,
                checkpoint.tracked_file_backups[a_file.to_str().unwrap()].source
            );
            assert_eq!(
                std::fs::read(
                    resumed.resolve_backup_path(stored.backup_file_name.as_ref().unwrap())
                )
                .unwrap(),
                b"A edited"
            );
            assert!(!later
                .tracked_file_backups
                .contains_key(b_file.to_str().unwrap()));
            resumed.rewind_files(first).await.unwrap();
            if had_backup {
                assert_eq!(std::fs::read(&a_file).unwrap(), b"A before");
            } else {
                assert!(!a_file.exists());
            }
            assert_eq!(std::fs::read(&b_file).unwrap(), b"B business data");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replaced_tracked_directory_never_retargets_snapshot_or_rewind() {
        for had_backup in [false, true] {
            for symlink in [false, true] {
                let (home, a) = scratch(&format!("replaced-parent-{had_backup}-{symlink}"));
                let b = a.parent().unwrap().join("other");
                let retired = a.parent().unwrap().join("retired");
                std::fs::create_dir(&b).unwrap();
                let a_file = a.join("foo");
                let b_file = b.join("foo");
                if had_backup {
                    std::fs::write(&a_file, b"A before").unwrap();
                }
                std::fs::write(&b_file, b"B business data").unwrap();
                let history = FileHistory::new(home, "replaced-parent".into());
                let first = Uuid::new_v4();
                history.make_snapshot(first).await;
                history.track_edit(a_file.to_str().unwrap()).await;
                std::fs::write(&a_file, b"A edited").unwrap();
                let captured = history.snapshot_record(first).unwrap();

                std::fs::rename(&a, &retired).unwrap();
                if symlink {
                    std::os::unix::fs::symlink(&b, &a).unwrap();
                } else {
                    std::fs::create_dir(&a).unwrap();
                    std::fs::write(&a_file, b"replacement business data").unwrap();
                }
                let later = history.make_snapshot(Uuid::new_v4()).await;
                assert_eq!(
                    later.tracked_file_backups[a_file.to_str().unwrap()],
                    captured.tracked_file_backups[a_file.to_str().unwrap()]
                );
                let result = history.rewind_files(first).await.unwrap();
                assert_eq!(result.skipped_links, 1);
                assert!(result.changed.is_empty());
                assert_eq!(std::fs::read(&b_file).unwrap(), b"B business data");
                assert_eq!(std::fs::read(retired.join("foo")).unwrap(), b"A edited");
                if !symlink {
                    assert_eq!(std::fs::read(a_file).unwrap(), b"replacement business data");
                }
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn new_file_with_missing_parents_keeps_existing_ancestor_authority() {
        let (home, a) = scratch("new-nested-parent");
        let b = a.parent().unwrap().join("other");
        let retired = a.parent().unwrap().join("retired-nested");
        std::fs::create_dir(&b).unwrap();
        std::fs::write(b.join("foo"), b"B business data").unwrap();
        let nested = a.join("nested");
        let path = nested.join("foo");
        let history = FileHistory::new(home, "missing-parent".into());
        let first = Uuid::new_v4();
        history.make_snapshot(first).await;
        history.track_edit(path.to_str().unwrap()).await;
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(&path, b"new A file").unwrap();
        std::fs::rename(&nested, &retired).unwrap();
        std::os::unix::fs::symlink(&b, &nested).unwrap();
        let outcome = history.rewind_files(first).await.unwrap();
        assert_eq!(outcome.skipped_links, 1);
        assert_eq!(std::fs::read(b.join("foo")).unwrap(), b"B business data");
        assert_eq!(std::fs::read(retired.join("foo")).unwrap(), b"new A file");
    }

    #[tokio::test]
    async fn post_write_receipt_rejects_parent_replaced_before_binding() {
        let (home, a) = scratch("replaced-before-write-receipt");
        let nested = a.join("nested");
        let retired = a.parent().unwrap().join("retired");
        let path = nested.join("foo");
        let history = FileHistory::new(home, "write-receipt".into());
        let first = Uuid::new_v4();
        history.make_snapshot(first).await;
        history.track_edit(path.to_str().unwrap()).await;
        let result = rooted_fs::write_file_after_permission(
            &a,
            Path::new("nested/foo"),
            &path,
            &path,
            b"new file",
        )
        .unwrap();
        let parent_receipt = result.parent.unwrap();
        std::fs::rename(&nested, &retired).unwrap();
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(&path, b"replacement business data").unwrap();
        history.track_completed_write(path.to_str().unwrap(), parent_receipt);
        let outcome = history.rewind_files(first).await.unwrap();
        assert_eq!(outcome.skipped_links, 1);
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement business data");
        assert_eq!(std::fs::read(retired.join("foo")).unwrap(), b"new file");
    }

    #[test]
    fn checkpoint_without_original_absolute_source_is_rejected() {
        let (home, cwd) = scratch("invalid-provenance");
        let message = Uuid::new_v4();
        let source = FileHistorySource::capture(&cwd.join("foo")).unwrap();
        for (path, provenance) in [
            ("foo".to_owned(), Some(source)),
            (cwd.join("foo").to_string_lossy().into_owned(), None),
        ] {
            let mut backup = serde_json::json!({"backupFileName": null, "version":1});
            if let Some(source) = provenance {
                backup["source"] = serde_json::to_value(source).unwrap();
            }
            let content = serde_json::json!({"type":"file-history-snapshot", "messageId":message,
                "trackedFileBackups": {path:backup}})
            .to_string();
            assert!(parse_snapshot_records(&content).is_err());
            let history = FileHistory::new(home.clone(), "invalid".into());
            assert!(!history.can_restore(message));
        }
    }

    #[tokio::test]
    async fn has_any_changes_and_missing_snapshot() {
        let (home, cwd) = scratch("changes");
        let file = cwd.join("b.txt");
        std::fs::write(&file, "x\n").unwrap();
        let fh = FileHistory::new(home, "sess3".into());
        let msg = Uuid::new_v4();
        fh.make_snapshot(msg).await;
        fh.track_edit(file.to_str().unwrap()).await;
        // Not yet edited → no change vs the backup.
        assert!(!fh.has_any_changes(msg).await);
        std::fs::write(&file, "y\n").unwrap();
        assert!(fh.has_any_changes(msg).await);
        // Unknown message → false, and rewind errors.
        assert!(!fh.has_any_changes(Uuid::new_v4()).await);
        assert!(fh.rewind_files(Uuid::new_v4()).await.is_err());
    }

    #[tokio::test]
    async fn pruning_snapshots_deletes_orphaned_backup_files() {
        let (home, cwd) = scratch("prune");
        let file = cwd.join("tracked.txt");
        std::fs::write(&file, "v0\n").unwrap();
        let fh = FileHistory::new(home.clone(), "sess-prune".into());

        let first_msg = Uuid::new_v4();
        fh.make_snapshot(first_msg).await;
        fh.track_edit(file.to_str().unwrap()).await;
        std::fs::write(&file, "v1\n").unwrap();

        let first_backup = FileHistory::backup_file_name(&file.to_string_lossy(), 1);
        let first_backup_path = home
            .join("file-history")
            .join("sess-prune")
            .join(&first_backup);
        assert!(
            first_backup_path.exists(),
            "v1 backup should exist before prune"
        );

        for idx in 0..MAX_SNAPSHOTS {
            fh.make_snapshot(Uuid::new_v4()).await;
            std::fs::write(&file, format!("v{}\n", idx + 2)).unwrap();
        }

        assert!(
            !first_backup_path.exists(),
            "pruning the first snapshot should delete its unreferenced backup"
        );
    }

    #[test]
    fn snapshot_rows_parse_even_when_the_type_is_escaped() {
        let (first, second) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let (_home, cwd) = scratch("escaped-type");
        let path = cwd.join("a.rs");
        let source = FileHistorySource::capture(&path).unwrap();
        let content = [
            serde_json::json!({"type":"file-history-snapshot", "messageId":first,
                "trackedFileBackups": { (path.to_str().unwrap()): {
                    "backupFileName":"a@v1", "version":1, "source":source
                }}
            }).to_string(),
            r#"{"type":"user","message":{"content":"file-history-snapshot"}}"#.to_owned(),
            format!(
                r#"{{"type":"file\u002dhistory-snapshot","messageId":"{second}","trackedFileBackups":{{}}}}"#
            ),
        ]
        .join("\n");
        let records = parse_snapshot_records(&content).unwrap();
        let ids = records.iter().map(|r| r.message_id).collect::<Vec<_>>();
        assert_eq!(ids, vec![first, second]);
        assert_eq!(
            records[0].tracked_file_backups[path.to_str().unwrap()]
                .backup_file_name
                .as_deref(),
            Some("a@v1")
        );
    }
}
