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

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Above this the oldest snapshots are evicted (claude-code `MAX_SNAPSHOTS`).
const MAX_SNAPSHOTS: usize = 100;

/// One file's backup at a given version. `backup_file_name = None` marks "the
/// file did not exist in this version" (so rewinding deletes it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHistoryBackup {
    /// The `<hash>@v<n>` backup file name, or `None` if the file was absent.
    pub backup_file_name: Option<String>,
    /// Monotonic per-file version.
    pub version: u32,
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
    /// Original cwd — tracking paths are stored relative to it when possible.
    cwd: PathBuf,
    /// Bare session uuid — the backup dir segment.
    session_id: String,
}

impl FileHistory {
    /// New store for `session_id` under `home`, with tracking paths shortened
    /// against `cwd`.
    #[must_use]
    pub fn new(home: PathBuf, cwd: PathBuf, session_id: String) -> Self {
        Self {
            state: Mutex::new(State::default()),
            home,
            cwd,
            session_id,
        }
    }

    /// Rebuild the in-memory index from persisted `file-history-snapshot`
    /// records (resume). Tracking paths are re-shortened; `tracked_files` is the
    /// union of every snapshot's keys (claude `fileHistoryRestoreStateFromLog`).
    pub fn restore_from_records(&self, records: Vec<SnapshotRecord>) {
        let mut tracked = HashSet::new();
        let mut snapshots = Vec::with_capacity(records.len());
        for rec in records {
            let mut backups = BTreeMap::new();
            for (path, backup) in rec.tracked_file_backups {
                let key = self.shorten(&path);
                tracked.insert(key.clone());
                backups.insert(key, backup);
            }
            snapshots.push(FileHistorySnapshot {
                message_id: rec.message_id,
                tracked_file_backups: backups,
            });
        }
        let mut st = self.state.lock().expect("file-history lock");
        st.snapshots = snapshots;
        st.tracked_files = tracked;
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
        let tracking_path = self.shorten(file_path);

        // Phase 1: is a backup needed? (already tracked in the most-recent
        // snapshot ⇒ skip; re-backing v1 would corrupt it with post-edit bytes.)
        {
            let st = self.state.lock().expect("file-history lock");
            let Some(recent) = st.snapshots.last() else {
                return;
            };
            if recent.tracked_file_backups.contains_key(&tracking_path) {
                return;
            }
        }

        // Phase 2: async backup (v1) outside the lock.
        let Ok(backup) = self.create_backup(Some(Path::new(file_path)), 1).await else {
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
            let file_path = self.expand(&tracking_path);
            let latest_backup = latest.get(&tracking_path);
            let next_version = latest_backup.map_or(1, |b| b.version + 1);

            let exists = tokio::fs::metadata(&file_path).await.is_ok();
            if !exists {
                backups.insert(
                    tracking_path,
                    FileHistoryBackup {
                        backup_file_name: None,
                        version: next_version,
                    },
                );
                continue;
            }
            // File exists — reuse the latest backup if unchanged, else re-back.
            if let Some(lb) = latest_backup {
                if lb.backup_file_name.is_some()
                    && !self
                        .origin_file_changed(&file_path, lb.backup_file_name.as_deref())
                        .await
                {
                    backups.insert(tracking_path, lb.clone());
                    continue;
                }
            }
            if let Ok(fresh) = self.create_backup(Some(&file_path), next_version).await {
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
        let (target, tracked, snapshots) = {
            let st = self.state.lock().expect("file-history lock");
            let target = st
                .snapshots
                .iter()
                .rev()
                .find(|s| s.message_id == message_id)
                .cloned();
            (
                target,
                st.tracked_files.iter().cloned().collect::<Vec<_>>(),
                st.snapshots.clone(),
            )
        };
        let Some(target) = target else {
            return Err("The selected snapshot was not found".to_string());
        };

        let mut outcome = RewindOutcome::default();
        for tracking_path in tracked {
            let file_path = self.expand(&tracking_path);
            // Resolve the backup for this file at the target version, falling
            // back to its first-version backup when untracked at the target.
            let backup_name: Option<Option<String>> =
                match target.tracked_file_backups.get(&tracking_path) {
                    Some(b) => Some(b.backup_file_name.clone()),
                    None => Self::first_version_backup(&snapshots, &tracking_path),
                };
            let Some(backup_name) = backup_name else {
                continue; // unresolved → leave the file untouched
            };
            match self.rewind_one(&file_path, backup_name.as_deref()).await {
                RewindStep::Unchanged => {}
                RewindStep::Changed(path) => outcome.changed.push(path),
                RewindStep::SkippedLink => outcome.skipped_links += 1,
                RewindStep::Failed => outcome.failed += 1,
            }
        }
        // 2.1.260: a rewind that could not restore ANY of the files that needed
        // restoring is not a success. The oracle spells this `canRewind:false`
        // and the CLI prints the error instead of "Files rewound to state at …".
        // A rewind where nothing differed has `changed == 0 && failed == 0` and
        // stays `Ok` — "nothing to do" is not a failure.
        if outcome.changed.is_empty() && outcome.failed > 0 {
            return Err(format!(
                "No files could be restored to the state at this message ({} file{} failed; \
                 their checkpoint backups are missing)",
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
    async fn rewind_one(&self, file_path: &Path, backup_file_name: Option<&str>) -> RewindStep {
        // 2.1.216 link safety, checked BEFORE either branch touches the path:
        // `fs::copy` writes THROUGH a destination symlink and `remove_file`
        // would unlink the link rather than the checkpointed file, so both
        // operations can escape the tree. `symlink_metadata` does not follow.
        match tokio::fs::symlink_metadata(file_path).await {
            Ok(meta) if !meta.is_file() => return RewindStep::SkippedLink,
            Ok(meta) if is_hard_linked(&meta) => return RewindStep::SkippedLink,
            // Absent is fine: a restore recreates it, a delete has nothing to do.
            _ => {}
        }
        match backup_file_name {
            None => {
                // Absent at the target version → delete if present.
                match tokio::fs::remove_file(file_path).await {
                    Ok(()) => RewindStep::Changed(file_path.to_string_lossy().into_owned()),
                    // Already gone ⇒ the tree already matches the target.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => RewindStep::Unchanged,
                    Err(_) => RewindStep::Failed,
                }
            }
            Some(name) => {
                if !self.origin_file_changed(file_path, Some(name)).await {
                    return RewindStep::Unchanged;
                }
                match self.restore_backup(file_path, name).await {
                    Ok(true) => RewindStep::Changed(file_path.to_string_lossy().into_owned()),
                    // The backup file itself is gone. Before this, `restore_backup`
                    // returned `Ok(())` here and the path was still pushed onto
                    // `changed` — the file was reported as restored having never
                    // been touched.
                    Ok(false) | Err(_) => RewindStep::Failed,
                }
            }
        }
    }

    /// Would rewinding to `message_id` change any file on disk? (the picker's
    /// per-row `has_code_changes` flag — early-exits, never diffs).
    #[must_use]
    pub async fn has_any_changes(&self, message_id: Uuid) -> bool {
        let (target, tracked, snapshots) = {
            let st = self.state.lock().expect("file-history lock");
            let target = st
                .snapshots
                .iter()
                .rev()
                .find(|s| s.message_id == message_id)
                .cloned();
            (
                target,
                st.tracked_files.iter().cloned().collect::<Vec<_>>(),
                st.snapshots.clone(),
            )
        };
        let Some(target) = target else {
            return false;
        };
        for tracking_path in tracked {
            let file_path = self.expand(&tracking_path);
            let backup_name = match target.tracked_file_backups.get(&tracking_path) {
                Some(b) => Some(b.backup_file_name.clone()),
                None => Self::first_version_backup(&snapshots, &tracking_path),
            };
            let Some(backup_name) = backup_name else {
                continue;
            };
            match backup_name {
                None => {
                    if tokio::fs::metadata(&file_path).await.is_ok() {
                        return true;
                    }
                }
                Some(name) => {
                    if self.origin_file_changed(&file_path, Some(&name)).await {
                        return true;
                    }
                }
            }
        }
        false
    }

    // ── helpers ──────────────────────────────────────────────────────────────

    /// The first-version (`v1`) backup name for `tracking_path` across all
    /// snapshots (used when the target snapshot predates the file's tracking).
    /// `Some(None)` = existed-as-absent; `None` = unresolved.
    fn first_version_backup(
        snapshots: &[FileHistorySnapshot],
        tracking_path: &str,
    ) -> Option<Option<String>> {
        for snap in snapshots {
            if let Some(b) = snap.tracked_file_backups.get(tracking_path) {
                if b.version == 1 {
                    return Some(b.backup_file_name.clone());
                }
            }
        }
        None
    }

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

    /// Copy `file_path`'s current content to a fresh backup; `None`/missing
    /// source records a null backup. Lazy-mkdir on ENOENT.
    async fn create_backup(
        &self,
        file_path: Option<&Path>,
        version: u32,
    ) -> Result<FileHistoryBackup, std::io::Error> {
        let Some(file_path) = file_path else {
            return Ok(FileHistoryBackup {
                backup_file_name: None,
                version,
            });
        };
        if tokio::fs::metadata(file_path).await.is_err() {
            return Ok(FileHistoryBackup {
                backup_file_name: None,
                version,
            });
        }
        let name = Self::backup_file_name(&file_path.to_string_lossy(), version);
        let backup_path = self.resolve_backup_path(&name);
        if tokio::fs::copy(file_path, &backup_path).await.is_err() {
            if let Some(parent) = backup_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::copy(file_path, &backup_path).await?;
        }
        Ok(FileHistoryBackup {
            backup_file_name: Some(name),
            version,
        })
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
    ) -> std::io::Result<bool> {
        let backup_path = self.resolve_backup_path(backup_file_name);
        if tokio::fs::metadata(&backup_path).await.is_err() {
            return Ok(false);
        }
        if tokio::fs::copy(&backup_path, file_path).await.is_err() {
            if let Some(parent) = file_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::copy(&backup_path, file_path).await?;
        }
        Ok(true)
    }

    /// Has `original` diverged from `backup_file_name`? Missing backup name ⇒
    /// treat as changed. Compares existence then byte content.
    async fn origin_file_changed(&self, original: &Path, backup_file_name: Option<&str>) -> bool {
        let Some(name) = backup_file_name else {
            return true;
        };
        let backup_path = self.resolve_backup_path(name);
        let orig_meta = tokio::fs::metadata(original).await;
        let back_meta = tokio::fs::metadata(&backup_path).await;
        match (orig_meta, back_meta) {
            (Ok(a), Ok(b)) if a.len() != b.len() => true,
            (Ok(_), Ok(_)) => files_differ(original, &backup_path).await,
            _ => true,
        }
    }

    /// Store an absolute path relative to `cwd` when it lives under it (claude
    /// `maybeShortenFilePath` — smaller keys); otherwise verbatim.
    fn shorten(&self, file_path: &str) -> String {
        let p = Path::new(file_path);
        if p.is_absolute() {
            if let Ok(rel) = p.strip_prefix(&self.cwd) {
                return rel.to_string_lossy().into_owned();
            }
        }
        file_path.to_string()
    }

    /// Re-absolutize a shortened tracking path against `cwd`.
    fn expand(&self, tracking_path: &str) -> PathBuf {
        let p = Path::new(tracking_path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cwd.join(p)
        }
    }
}

/// Does this `symlink_metadata` describe a file with more than one hard link?
///
/// A tracked path that is one of several names for the same inode must not be
/// restored or deleted through: `fs::copy` would rewrite the shared inode's
/// content and `remove_file` would drop one name while the others keep the
/// post-edit content — either way a file OUTSIDE the checkpoint changes
/// (oracle 2.1.216: "a symlink, hard link, or other non-regular file").
#[cfg(unix)]
fn is_hard_linked(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.nlink() > 1
}

/// Windows has no portable link count through `Metadata`; symlinks and other
/// non-regular files are still refused by the `is_file` arm at the call site.
#[cfg(not(unix))]
fn is_hard_linked(_meta: &std::fs::Metadata) -> bool {
    false
}

async fn files_differ(original: &Path, backup: &Path) -> bool {
    use tokio::io::AsyncReadExt;
    let Ok(mut orig) = tokio::fs::File::open(original).await else {
        return true;
    };
    let Ok(mut back) = tokio::fs::File::open(backup).await else {
        return true;
    };
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

/// Parse every persisted `file-history-snapshot` line out of a transcript body.
#[must_use]
pub fn parse_snapshot_records(content: &str) -> Vec<SnapshotRecord> {
    let mut out = Vec::new();
    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("type").and_then(serde_json::Value::as_str) != Some("file-history-snapshot") {
            continue;
        }
        let Some(message_id) = v
            .get("messageId")
            .and_then(serde_json::Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            continue;
        };
        let mut backups = BTreeMap::new();
        if let Some(obj) = v
            .get("trackedFileBackups")
            .and_then(serde_json::Value::as_object)
        {
            for (path, b) in obj {
                let version = b
                    .get("version")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1) as u32;
                let backup_file_name = b
                    .get("backupFileName")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                backups.insert(
                    path.clone(),
                    FileHistoryBackup {
                        backup_file_name,
                        version,
                    },
                );
            }
        }
        out.push(SnapshotRecord {
            message_id,
            tracked_file_backups: backups,
        });
    }
    out
}

/// The message ids that have a persisted checkpoint (the `/rewind` picker's
/// candidate rows), in transcript order.
#[must_use]
pub fn checkpoint_message_ids(content: &str) -> Vec<Uuid> {
    parse_snapshot_records(content)
        .into_iter()
        .map(|r| r.message_id)
        .collect()
}

/// Rebuild a [`FileHistory`] from `session_id`'s persisted transcript and rewind
/// the working tree to `message_id`. Used by the CLI restore path AFTER the TUI
/// has unwound (the live in-memory index is gone by then).
///
/// # Errors
/// Propagates [`FileHistory::rewind_files`]'s target-not-found error.
pub async fn rewind_from_disk(
    home: &Path,
    cwd: &str,
    session_id: Uuid,
    message_id: Uuid,
) -> Result<RewindOutcome, String> {
    let path = crate::jsonl::session_path(home, cwd, &session_id.to_string());
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("read transcript: {e}"))?;
    let records = parse_snapshot_records(&content);
    let fh = FileHistory::new(
        home.to_path_buf(),
        PathBuf::from(cwd),
        session_id.to_string(),
    );
    fh.restore_from_records(records);
    fh.rewind_files(message_id).await
}

#[async_trait::async_trait]
impl platform_api::FileHistorySink for FileHistory {
    async fn track_edit(&self, file_path: &str) {
        FileHistory::track_edit(self, file_path).await;
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
        (home, cwd)
    }

    #[tokio::test]
    async fn edit_then_rewind_restores_pre_edit_content() {
        let (home, cwd) = scratch("restore");
        let file = cwd.join("a.txt");
        std::fs::write(&file, "v0\n").unwrap();
        let fh = FileHistory::new(home, cwd.clone(), "sess1".into());

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
        let fh = FileHistory::new(home.clone(), cwd.clone(), "sess-mb".into());

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
        let fh = FileHistory::new(home, cwd.clone(), "sess-sym".into());

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
        let fh = FileHistory::new(home, cwd.clone(), "sess-symdel".into());
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
        let fh = FileHistory::new(home, cwd.clone(), "sess-noop".into());
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
        let fh = FileHistory::new(home, cwd.clone(), "sess2".into());
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
        let fh = FileHistory::new(home.clone(), cwd.clone(), "sess-rt".into());
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
        let fresh = FileHistory::new(home, cwd, "sess-rt".into());
        fresh.restore_from_records(parse_snapshot_records(&content));

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
        let first = FileHistory::new(home.clone(), cwd.clone(), "sess-resume".into());
        let first_message = Uuid::new_v4();
        first.make_snapshot(first_message).await;
        first.track_edit(file.to_str().unwrap()).await;
        std::fs::write(&file, "after first turn\n").unwrap();
        let first_record = first.snapshot_record(first_message).unwrap();
        let content = format!(
            "{}\n",
            serde_json::to_string(&snapshot_line_json("sess-resume", &first_record)).unwrap()
        );

        let resumed = FileHistory::new(home, cwd, "sess-resume".into());
        resumed.restore_from_records(parse_snapshot_records(&content));
        let second_message = Uuid::new_v4();
        let second_record = resumed.make_snapshot(second_message).await;
        let backup = second_record
            .tracked_file_backups
            .get("note.txt")
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
    async fn has_any_changes_and_missing_snapshot() {
        let (home, cwd) = scratch("changes");
        let file = cwd.join("b.txt");
        std::fs::write(&file, "x\n").unwrap();
        let fh = FileHistory::new(home, cwd.clone(), "sess3".into());
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
        let fh = FileHistory::new(home.clone(), cwd.clone(), "sess-prune".into());

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
}
