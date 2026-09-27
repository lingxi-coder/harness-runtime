//! Atomic on-disk storage for local apps (spec §D).
//!
//! Layout under the injected data root:
//!
//! ```text
//! apps/index.json                          — { schemaVersion, apps }
//! apps/<app-id>/runtime.json               — AppRuntimeRecord
//! apps/<app-id>/dependencies.json          — AppDependencyRecord
//! apps/<app-id>/workspace/.lingxi/app.json — { schemaVersion, app } mirror
//! ```
//!
//! Every write goes through [`platform_api::rooted_fs::atomic_write`] (same-directory
//! temp file + rename, no symlink traversal below the root), so a crash can
//! only ever leave an orphan `*.tmp-<pid>-<seq>` file behind — loaders address
//! exact file names and therefore ignore orphans naturally. Every document
//! carries `schemaVersion`; enumeration is index-driven (an app directory not
//! listed in `index.json` is invisible), which makes `index.json` the commit
//! point for creation (per-app files first, index last) and deletion (index
//! entry first, directory last).
//!
//! Index-driven enumeration has one cost: a creation that never REACHED that
//! commit point leaves an `apps/<id>` nothing can see again, holding its id
//! hostage through [`app_id_present_on_disk`]. The create path therefore lays
//! down a [`CREATING_MARKER_FILE`] before the first skeleton byte and removes
//! it after the index commit, and [`load_all`] reclaims marked directories the
//! index does not list through [`sweep_uncommitted_app_dirs`].
//!
//! For MUTATIONS of a listed app the per-app batch is the effective commit
//! point instead: the batch lands `runtime.json` and finally the `app.json`
//! record mirror — the order is defined ONCE as [`APP_DOC_WRITE_ORDER`] and
//! executed by [`save_app_files_steps`] — and only then does the caller
//! rewrite the index. The repair contract depends on that ORDER, not on
//! completeness: a writer may skip documents that did not change (the
//! service's mutation path does), but the writes that DO happen must follow
//! the canonical sequence. [`load_all`] reconciles on load: a diverged
//! `app.json` mirror (written after every other per-app document) supersedes
//! the index record. Repairs are persisted through the normal write path
//! before the load returns.
//!
//! Legacy pipeline documents (`interactions.json`, `design-spec.json`) from
//! pre-v3 stores are simply IGNORED — never read, never deleted. Stale
//! `workflowState` fields are now inert unknown JSON and no longer participate
//! in persistence or load-time repair.
//!
//! Cross-process coordination: `apps/index.json` is read-modify-written under
//! the advisory `apps/index.lock` file lock ([`lock_exclusive`]-style, the
//! same pattern sibling crates use for shared spool files). [`load_all`] holds
//! it across the whole load (read + repair persists) and
//! [`save_index_preserving`] holds it across its re-read + merge + write, so
//! two service instances over one root cannot clobber each other's index
//! entries. Per-app build promotion, checkpoint mutation, and deletion share
//! `apps/<id>/build.lock`, while host-owned background task claims use the
//! independent `apps/<id>/background.lock`, so a second engine process cannot
//! execute the same durable flow concurrently. Deletion is
//! rename-to-trash: `apps/<id>` is atomically renamed
//! into `apps/.trash/<id>-<nonce>` (the commit point AND the tombstone —
//! `rename` never follows the final component, and a racing create cannot
//! collide while either the live dir or the tombstone exists), then removed;
//! leftovers in `apps/.trash` are swept best-effort at the next load.
//! [`load_all`] itself is index-driven and never enumerates `apps/`, so
//! `.trash` is invisible to it beyond the sweep.
//!
//! A first scaffold has one additional crash boundary.  The host writes a
//! durable journal and a complete copy of the shell before it stamps the
//! manifest or wipes the interview workspace.  [`load_all`] resolves that
//! journal while the index lock is held and *before* it reads any per-app
//! document.  A journal whose commit point did not land restores the shell;
//! one whose record commit did land is only cleaned up.  This keeps a crash
//! between the manifest/workspace writes and `scaffolded = true` from ever
//! becoming an authoritative, routable half-app.

use crate::error::AppError;
use crate::ids;
use crate::state::AppState;
use crate::types::{
    AppDependencyRecord, AppDependencyState, AppRecord, AppRuntimeRecord, AppRuntimeState,
    APPS_SCHEMA_VERSION,
};
use platform_api::rooted_fs::{self, AtomicWriteOptions};
use platform_api::FsError;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Upper bound on any single persisted document, enforced in BOTH directions:
/// loads refuse to slurp a larger file (typed `storage_corrupt` — the file is
/// out of contract), and [`save_app_files_steps`]/`write_doc` refuse to
/// produce one (typed `invalid_request`, checked BEFORE any temp file is
/// written). The write seam is where the invariant is enforced: nothing this
/// module persists can later fail its own load on size.
pub const MAX_DOC_BYTES: u64 = 8 * 1024 * 1024;

/// Directory under the data root holding all app state.
pub const APPS_DIR: &str = "apps";
/// Index document listing every app.
pub const INDEX_FILE: &str = "index.json";
/// Advisory lock file serializing every `apps/index.json` read-modify-write
/// transaction across processes (and across service instances in one
/// process). A runtime artifact, not a persisted document — loaders never
/// read it.
pub const INDEX_LOCK_FILE: &str = "index.lock";
/// Per-app advisory lock serializing build promotion and physical deletion.
/// This is a runtime artifact, not a persisted document.
pub const BUILD_LOCK_FILE: &str = "build.lock";
/// Per-app advisory lock serializing host-owned background task claim and
/// terminal state transitions across foreground/headless engine instances.
/// This is a runtime artifact, not a persisted document.
pub const BACKGROUND_LOCK_FILE: &str = "background.lock";
/// Tombstone directory for deleted app dirs (`apps/.trash`). Never a legal
/// app id (ids cannot start with `.`), invisible to the index-driven
/// [`load_all`], swept best-effort at load.
pub const TRASH_DIR: &str = ".trash";
/// Per-app runtime record.
pub const RUNTIME_FILE: &str = "runtime.json";
/// Per-app dependency-install record.
pub const DEPENDENCY_FILE: &str = "dependencies.json";
/// Per-app workspace directory (the Next.js project root).
pub const WORKSPACE_DIR: &str = "workspace";
/// App-scoped state directory inside the workspace (`.lingxi`).
pub const APP_STATE_DIR: &str = branding::DOT_DIR;
/// App-scoped metadata mirror inside `workspace/.lingxi/`.
pub const APP_METADATA_FILE: &str = "app.json";
/// Sibling directory holding temporary, durable first-scaffold snapshots.
/// Entries are removed after commit/rollback; an entry left by a crash is
/// consumed by [`load_all`] before the app is made available to the service.
pub const SCAFFOLD_RECOVERY_DIR: &str = ".scaffold-recovery";
/// Journal inside `apps/<id>` describing an in-flight first scaffold.
pub const SCAFFOLD_RECOVERY_JOURNAL_FILE: &str = "scaffold-recovery.json";
/// Marker written inside `apps/<id>` BEFORE the create skeleton and removed
/// after the index commit that publishes the app. A runtime artifact, not a
/// persisted document — loaders never read its contents.
///
/// It exists so [`sweep_uncommitted_app_dirs`] can tell the one directory
/// shape it is allowed to reclaim (a create that never reached its commit
/// point) apart from every other directory that happens to be missing from
/// the index. Without it the sweep would have to reclaim ANY unindexed
/// `apps/<id>`, which would delete a live app's tree the moment the index
/// went missing for some other reason.
pub const CREATING_MARKER_FILE: &str = "creating.marker";
/// File name for the advisory lock serializing first-scaffold recovery with
/// the store loader. The lock lives in a private OS temporary directory, not
/// under the app store: it is runtime coordination state and must not appear
/// in a persisted-store snapshot or schema round-trip.
pub const SCAFFOLD_RECOVERY_LOCK_FILE: &str = "scaffold-recovery.lock";

/// Durable first-scaffold recovery metadata.
///
/// The backup is a complete copy of the app directory (apart from advisory
/// lock files and this journal).  The target fields let load-time recovery
/// distinguish a committed mirror from a stale shell without trusting a
/// partially written manifest.  This is intentionally a storage primitive,
/// not a profile-selection primitive: the host remains the authority for the
/// runtime binding and the service's `scaffolded` bit remains the commit point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScaffoldRecoveryJournal {
    /// Store schema this journal belongs to.
    pub schema_version: u32,
    /// App whose directory was snapshotted.
    pub app_id: String,
    /// File-name component under [`SCAFFOLD_RECOVERY_DIR`].
    pub backup_name: String,
    /// Proposed record identity used to recognize a durable commit.
    pub target_name: String,
    /// Proposed record description used to recognize a durable commit.
    pub target_brief: String,
}

/// Handle for one in-flight first-scaffold transaction.
///
/// The handle is returned only after both the complete backup and journal have
/// been durably written.  Callers must consume it with [`Self::commit`] after
/// the service record commit, or [`Self::rollback`] on every earlier failure.
#[derive(Debug)]
pub struct ScaffoldRecoveryHandle {
    root: PathBuf,
    app_id: String,
    backup_name: String,
}

/// The whole `apps/index.json` document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppIndexFile {
    /// Persisted schema version ([`APPS_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// Every app, in creation order.
    pub apps: Vec<AppRecord>,
}

/// The `workspace/.lingxi/app.json` app-scoped metadata mirror.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppMetadataFile {
    /// Persisted schema version ([`APPS_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// Mirror of this app's index record.
    pub app: AppRecord,
}

/// Root-relative path of `apps/index.json`.
#[must_use]
pub fn index_rel() -> PathBuf {
    PathBuf::from(APPS_DIR).join(INDEX_FILE)
}

/// Root-relative path of the advisory index lock (`apps/index.lock`).
#[must_use]
pub fn index_lock_rel() -> PathBuf {
    PathBuf::from(APPS_DIR).join(INDEX_LOCK_FILE)
}

/// Root-relative path of the deletion tombstone dir (`apps/.trash`).
#[must_use]
pub fn trash_dir_rel() -> PathBuf {
    PathBuf::from(APPS_DIR).join(TRASH_DIR)
}

/// Take the advisory exclusive lock guarding `apps/index.json`
/// read-modify-write transactions. BLOCKS until acquired; callers must not
/// nest it (a second acquisition from the same process deadlocks — `flock`
/// excludes across open descriptions, not just across processes).
fn lock_index(root: &Path) -> Result<rooted_fs::RootedFileLock, AppError> {
    rooted_fs::lock_exclusive(
        root,
        &index_lock_rel(),
        rooted_fs::PRIVATE_DIR_MODE,
        rooted_fs::PRIVATE_FILE_MODE,
    )
    .map_err(|error| AppError::from_fs("lock apps index", &error))
}

/// Root-relative path of `apps/<id>`.
#[must_use]
pub fn app_dir_rel(app_id: &str) -> PathBuf {
    PathBuf::from(APPS_DIR).join(app_id)
}

/// Root-relative path of the uncommitted-create marker (`apps/<id>/creating.marker`).
#[must_use]
pub fn creating_marker_rel(app_id: &str) -> PathBuf {
    app_dir_rel(app_id).join(CREATING_MARKER_FILE)
}

/// Root-relative path of the per-app build/deletion lock.
#[must_use]
pub fn build_lock_rel(app_id: &str) -> PathBuf {
    app_dir_rel(app_id).join(BUILD_LOCK_FILE)
}

/// Root-relative path of the per-app background execution lock.
#[must_use]
pub fn background_lock_rel(app_id: &str) -> PathBuf {
    app_dir_rel(app_id).join(BACKGROUND_LOCK_FILE)
}

/// Take the advisory lock shared by local-app builds and physical deletion.
///
/// Callers must hold this lock for the complete operation that mutates or
/// removes an app's workspace/build tree. The app directory must already
/// exist; unlike the index lock, this helper deliberately does not create an
/// app directory as a side effect.
pub fn lock_app_build(root: &Path, app_id: &str) -> Result<rooted_fs::RootedFileLock, AppError> {
    ids::validate_app_id(app_id)?;
    let app_dir = rooted_fs::checked_join(root, &app_dir_rel(app_id))
        .map_err(|error| AppError::from_fs("lock app build", &error))?;
    match std::fs::symlink_metadata(&app_dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(AppError::StorageCorrupt(format!(
                "{} is not a real directory",
                app_dir.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(AppError::NotFound(format!("app {app_id}")));
        }
        Err(error) => {
            return Err(AppError::Io(format!(
                "inspect {} before locking: {error}",
                app_dir.display()
            )));
        }
    }
    rooted_fs::lock_exclusive(
        root,
        &build_lock_rel(app_id),
        rooted_fs::PRIVATE_DIR_MODE,
        rooted_fs::PRIVATE_FILE_MODE,
    )
    .map_err(|error| match error {
        FsError::NotFound(_) => AppError::NotFound(format!("app {app_id}")),
        other => AppError::from_fs("lock app build", &other),
    })
}

/// Take the advisory lock shared by host-owned background execution.
///
/// This lock is independent from `build.lock`: a long-running headless step
/// must not block an unrelated build, while two engine instances must never
/// claim the same durable task.
pub fn lock_app_background(
    root: &Path,
    app_id: &str,
) -> Result<rooted_fs::RootedFileLock, AppError> {
    ids::validate_app_id(app_id)?;
    let app_dir = rooted_fs::checked_join(root, &app_dir_rel(app_id))
        .map_err(|error| AppError::from_fs("lock app background", &error))?;
    match std::fs::symlink_metadata(&app_dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(AppError::StorageCorrupt(format!(
                "{} is not a real directory",
                app_dir.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(AppError::NotFound(format!("app {app_id}")));
        }
        Err(error) => {
            return Err(AppError::Io(format!(
                "inspect {} before locking: {error}",
                app_dir.display()
            )));
        }
    }
    rooted_fs::lock_exclusive(
        root,
        &background_lock_rel(app_id),
        rooted_fs::PRIVATE_DIR_MODE,
        rooted_fs::PRIVATE_FILE_MODE,
    )
    .map_err(|error| match error {
        FsError::NotFound(_) => AppError::NotFound(format!("app {app_id}")),
        other => AppError::from_fs("lock app background", &other),
    })
}

fn lock_app_build_if_present(
    root: &Path,
    app_id: &str,
) -> Result<Option<rooted_fs::RootedFileLock>, AppError> {
    ids::validate_app_id(app_id)?;
    let app_dir = rooted_fs::checked_join(root, &app_dir_rel(app_id))
        .map_err(|error| AppError::from_fs("lock app deletion", &error))?;
    match std::fs::symlink_metadata(&app_dir) {
        Ok(metadata) if metadata.is_dir() => match lock_app_build(root, app_id) {
            Ok(lock) => Ok(Some(lock)),
            Err(AppError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        },
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::Io(format!(
            "inspect {} before deleting: {error}",
            app_dir.display()
        ))),
    }
}

fn lock_app_background_if_present(
    root: &Path,
    app_id: &str,
) -> Result<Option<rooted_fs::RootedFileLock>, AppError> {
    ids::validate_app_id(app_id)?;
    let app_dir = rooted_fs::checked_join(root, &app_dir_rel(app_id))
        .map_err(|error| AppError::from_fs("lock app background deletion", &error))?;
    match std::fs::symlink_metadata(&app_dir) {
        Ok(metadata) if metadata.is_dir() => match lock_app_background(root, app_id) {
            Ok(lock) => Ok(Some(lock)),
            Err(AppError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        },
        Ok(_) => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::Io(format!(
            "inspect {} before deleting: {error}",
            app_dir.display()
        ))),
    }
}

/// Root-relative path of `apps/<id>/runtime.json`.
#[must_use]
pub fn runtime_rel(app_id: &str) -> PathBuf {
    app_dir_rel(app_id).join(RUNTIME_FILE)
}

/// Root-relative path of `apps/<id>/dependencies.json`.
#[must_use]
pub fn dependency_rel(app_id: &str) -> PathBuf {
    app_dir_rel(app_id).join(DEPENDENCY_FILE)
}

/// Root-relative path of `apps/<id>/workspace`.
#[must_use]
pub fn workspace_dir_rel(app_id: &str) -> PathBuf {
    app_dir_rel(app_id).join(WORKSPACE_DIR)
}

/// The `workspace_rel` string stored on [`AppRecord`]: always forward-slash
/// `apps/<id>/workspace`, independent of platform.
#[must_use]
pub fn workspace_rel_str(app_id: &str) -> String {
    format!("{APPS_DIR}/{app_id}/{WORKSPACE_DIR}")
}

/// The dependency record used for a newly-created app or a legacy app whose
/// dependency file predates this record.
#[must_use]
pub fn default_dependency_record(app_id: &str, now_ms: u64) -> AppDependencyRecord {
    AppDependencyRecord {
        schema_version: APPS_SCHEMA_VERSION,
        app_id: app_id.to_string(),
        state: AppDependencyState::Queued,
        lockfile_sha256: None,
        toolchain_key: None,
        install_attempts: 0,
        last_error: None,
        updated_at_ms: now_ms,
    }
}

fn derived_dependency_record(root: &Path, record: &AppRecord) -> AppDependencyRecord {
    let vite = root
        .join(workspace_dir_rel(&record.id))
        .join("node_modules/vite/bin/vite.js");
    let state = match std::fs::symlink_metadata(&vite) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            AppDependencyState::Ready
        }
        _ => AppDependencyState::Queued,
    };
    AppDependencyRecord {
        state,
        ..default_dependency_record(&record.id, record.updated_at_ms)
    }
}

/// Root-relative path of `apps/<id>/workspace/.lingxi/app.json`.
#[must_use]
pub fn metadata_rel(app_id: &str) -> PathBuf {
    workspace_dir_rel(app_id)
        .join(APP_STATE_DIR)
        .join(APP_METADATA_FILE)
}

/// Root-relative directory containing first-scaffold backups.
#[must_use]
pub fn scaffold_recovery_dir_rel() -> PathBuf {
    PathBuf::from(APPS_DIR).join(SCAFFOLD_RECOVERY_DIR)
}

/// Root-relative journal for an in-flight first scaffold.
#[must_use]
pub fn scaffold_recovery_journal_rel(app_id: &str) -> PathBuf {
    app_dir_rel(app_id).join(SCAFFOLD_RECOVERY_JOURNAL_FILE)
}

/// Logical lock file name used by [`lock_scaffold_recovery`]. This is retained
/// as a small naming helper for callers that need to describe the runtime
/// artifact; it is not a path inside the app store.
#[must_use]
pub fn scaffold_recovery_lock_rel() -> PathBuf {
    PathBuf::from(SCAFFOLD_RECOVERY_LOCK_FILE)
}

/// Return the private, per-store directory used for the recovery lock.
///
/// The lock must coordinate independent engine processes without creating a
/// new persisted file below the Local App store. Hashing the canonical store
/// root gives every process the same lock name while keeping user paths and
/// app identifiers out of the temporary directory. A pre-existing root is
/// expected by every normal caller; the fallback keeps direct test callers
/// and first-run setup deterministic when canonicalization cannot resolve it.
fn scaffold_recovery_lock_root(root: &Path) -> PathBuf {
    let identity = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let digest = Sha256::digest(identity.to_string_lossy().as_bytes());
    std::env::temp_dir().join(format!("lingxi-local-app-scaffold-{:x}", digest))
}

/// Take the global first-scaffold recovery lock. The lock is deliberately
/// outside `root` so read-only store loads do not leave a new document-like
/// artifact behind. Callers must take this before `index.lock` or any app
/// build lock when they may inspect or mutate scaffold recovery state.
pub fn lock_scaffold_recovery(root: &Path) -> Result<rooted_fs::RootedFileLock, AppError> {
    let lock_root = scaffold_recovery_lock_root(root);
    private_dir(&lock_root)?;
    rooted_fs::lock_exclusive(
        &lock_root,
        &scaffold_recovery_lock_rel(),
        rooted_fs::PRIVATE_DIR_MODE,
        rooted_fs::PRIVATE_FILE_MODE,
    )
    .map_err(|error| AppError::from_fs("lock scaffold recovery", &error))
}

fn scaffold_recovery_backup_rel(backup_name: &str) -> PathBuf {
    scaffold_recovery_dir_rel().join(backup_name)
}

fn private_dir(path: &Path) -> Result<(), AppError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(AppError::StorageCorrupt(format!(
            "{} is not a real directory",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(path)
                .map_err(|error| AppError::Io(format!("create {}: {error}", path.display())))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(
                    |error| AppError::Io(format!("restrict {}: {error}", path.display())),
                )?;
            }
            Ok(())
        }
        Err(error) => Err(AppError::Io(format!("inspect {}: {error}", path.display()))),
    }
}

fn remove_owned_path(path: &Path) -> Result<(), AppError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::remove_dir_all(path)
                .map_err(|error| AppError::Io(format!("remove {}: {error}", path.display())))
        }
        Ok(_) => std::fs::remove_file(path)
            .map_err(|error| AppError::Io(format!("remove {}: {error}", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::Io(format!("inspect {}: {error}", path.display()))),
    }
}

/// The relative target of a symlink inside a snapshot tree, refused unless it
/// stays inside `tree_root`.
///
/// LEXICAL, not `canonicalize`: a legitimate link can dangle (a `.bin` shim
/// whose target has not been written yet), and a snapshot must be judged on
/// what it SPELLS, not on what happens to exist right now. The link's own
/// position inside the tree is part of the arithmetic, so
/// `deps/.bin/vite -> ../vite/bin/vite.js` is contained while
/// `deps/../../../etc/passwd` is not.
///
/// Absolute targets are refused outright even when they currently point inside
/// the tree: the snapshot is copied to a DIFFERENT path, so an absolute link
/// would silently keep pointing at the original app's directory.
fn contained_symlink_target(link: &Path, tree_root: &Path) -> Result<PathBuf, AppError> {
    use std::path::Component;

    let escape = |reason: &str, target: &Path| {
        AppError::StorageCorrupt(format!(
            "scaffold recovery snapshot symlink {} -> {} {reason}",
            link.display(),
            target.display()
        ))
    };
    let target = std::fs::read_link(link)
        .map_err(|error| AppError::Io(format!("read link {}: {error}", link.display())))?;
    if target.is_absolute() {
        return Err(escape("must be relative", &target));
    }
    let parent_rel = link
        .parent()
        .and_then(|parent| parent.strip_prefix(tree_root).ok())
        .ok_or_else(|| {
            AppError::StorageCorrupt(format!(
                "scaffold recovery snapshot symlink {} is outside {}",
                link.display(),
                tree_root.display()
            ))
        })?;
    let mut resolved: Vec<std::ffi::OsString> = Vec::new();
    for component in parent_rel.components().chain(target.components()) {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if resolved.pop().is_none() {
                    return Err(escape("escapes the snapshot", &target));
                }
            }
            Component::Normal(name) => resolved.push(name.to_os_string()),
            Component::RootDir | Component::Prefix(_) => {
                return Err(escape("must be relative", &target));
            }
        }
    }
    Ok(target)
}

#[cfg(unix)]
fn recreate_snapshot_symlink(target: &Path, destination: &Path) -> Result<(), AppError> {
    std::os::unix::fs::symlink(target, destination).map_err(|error| {
        AppError::Io(format!(
            "recreate scaffold recovery symlink {} -> {}: {error}",
            destination.display(),
            target.display()
        ))
    })
}

#[cfg(not(unix))]
fn recreate_snapshot_symlink(target: &Path, destination: &Path) -> Result<(), AppError> {
    Err(AppError::StorageCorrupt(format!(
        "scaffold recovery snapshot symlink {} -> {} is unsupported on this platform",
        destination.display(),
        target.display()
    )))
}

/// Copy one entry of a snapshot tree. `source_root` is the root of the tree
/// being copied (the app directory when taking the backup, the backup when
/// restoring it) and is what symlink containment is judged against — passing
/// the directory currently being walked instead would let a nested link climb
/// out one level at a time.
fn copy_tree_entry(source: &Path, destination: &Path, source_root: &Path) -> Result<(), AppError> {
    let metadata = std::fs::symlink_metadata(source)
        .map_err(|error| AppError::Io(format!("inspect {}: {error}", source.display())))?;
    if metadata.file_type().is_symlink() {
        // Reproduced, not followed and not refused. Refusing was the old
        // behaviour and it made a workspace containing ANY symlink
        // permanently unscaffoldable: this snapshot is taken BEFORE the wipe
        // that would have removed the link, so the shell could never get past
        // its first scaffold.
        let target = contained_symlink_target(source, source_root)?;
        if let Some(parent) = destination.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    AppError::Io(format!("create {}: {error}", parent.display()))
                })?;
            }
        }
        return recreate_snapshot_symlink(&target, destination);
    }
    if metadata.is_dir() {
        private_dir(destination)?;
        for entry in std::fs::read_dir(source)
            .map_err(|error| AppError::Io(format!("read {}: {error}", source.display())))?
        {
            let entry = entry.map_err(|error| {
                AppError::Io(format!("read {} entry: {error}", source.display()))
            })?;
            copy_tree_entry(
                &entry.path(),
                &destination.join(entry.file_name()),
                source_root,
            )?;
        }
        return Ok(());
    }
    if !metadata.is_file() {
        return Err(AppError::StorageCorrupt(format!(
            "scaffold recovery snapshot contains a special file {}",
            source.display()
        )));
    }
    if let Some(parent) = destination.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)
                .map_err(|error| AppError::Io(format!("create {}: {error}", parent.display())))?;
        }
    }
    std::fs::copy(source, destination).map_err(|error| {
        AppError::Io(format!(
            "copy scaffold recovery file {} -> {}: {error}",
            source.display(),
            destination.display()
        ))
    })?;
    Ok(())
}

fn copy_app_snapshot(source: &Path, destination: &Path) -> Result<(), AppError> {
    let metadata = std::fs::symlink_metadata(source)
        .map_err(|error| AppError::Io(format!("inspect {}: {error}", source.display())))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(AppError::StorageCorrupt(format!(
            "app directory {} is not a real directory",
            source.display()
        )));
    }
    private_dir(destination)?;
    for entry in std::fs::read_dir(source)
        .map_err(|error| AppError::Io(format!("read {}: {error}", source.display())))?
    {
        let entry = entry
            .map_err(|error| AppError::Io(format!("read {} entry: {error}", source.display())))?;
        let name = entry.file_name();
        // These are live advisory locks held by the transaction itself (or by
        // another host operation). Copying them would snapshot stale lock
        // state and can make a restored app look busy forever.
        if name == std::ffi::OsStr::new(BUILD_LOCK_FILE)
            || name == std::ffi::OsStr::new(BACKGROUND_LOCK_FILE)
            || name == std::ffi::OsStr::new(SCAFFOLD_RECOVERY_JOURNAL_FILE)
        {
            continue;
        }
        copy_tree_entry(&entry.path(), &destination.join(name), source)?;
    }
    Ok(())
}

fn clear_app_directory_for_restore(app_dir: &Path) -> Result<(), AppError> {
    let metadata = std::fs::symlink_metadata(app_dir)
        .map_err(|error| AppError::Io(format!("inspect {}: {error}", app_dir.display())))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(AppError::StorageCorrupt(format!(
            "app directory {} is not a real directory",
            app_dir.display()
        )));
    }
    for entry in std::fs::read_dir(app_dir)
        .map_err(|error| AppError::Io(format!("read {}: {error}", app_dir.display())))?
    {
        let entry = entry
            .map_err(|error| AppError::Io(format!("read {} entry: {error}", app_dir.display())))?;
        let name = entry.file_name();
        if name == std::ffi::OsStr::new(BUILD_LOCK_FILE)
            || name == std::ffi::OsStr::new(BACKGROUND_LOCK_FILE)
            || name == std::ffi::OsStr::new(SCAFFOLD_RECOVERY_JOURNAL_FILE)
        {
            continue;
        }
        remove_owned_path(&entry.path())?;
    }
    Ok(())
}

fn restore_app_snapshot(root: &Path, journal: &ScaffoldRecoveryJournal) -> Result<(), AppError> {
    let app_dir = rooted_fs::checked_join(root, &app_dir_rel(&journal.app_id))
        .map_err(|error| AppError::from_fs("restore scaffold app", &error))?;
    let backup = rooted_fs::checked_join(root, &scaffold_recovery_backup_rel(&journal.backup_name))
        .map_err(|error| AppError::from_fs("restore scaffold backup", &error))?;
    let backup_name = backup
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            AppError::StorageCorrupt("scaffold recovery backup name is invalid".into())
        })?;
    if backup_name != journal.backup_name
        || !journal
            .backup_name
            .starts_with(&format!("{}-", journal.app_id))
    {
        return Err(AppError::StorageCorrupt(
            "scaffold recovery backup is not bound to its app".into(),
        ));
    }
    let backup_metadata = std::fs::symlink_metadata(&backup).map_err(|error| {
        AppError::StorageCorrupt(format!(
            "scaffold recovery backup {} is unavailable: {error}",
            backup.display()
        ))
    })?;
    if !backup_metadata.is_dir() || backup_metadata.file_type().is_symlink() {
        return Err(AppError::StorageCorrupt(format!(
            "scaffold recovery backup {} is not a real directory",
            backup.display()
        )));
    }
    // Validate the complete source before removing the live app. If the
    // backup is damaged, fail closed and leave the journal for an operator;
    // never turn a corrupted snapshot into a silently empty app.
    validate_snapshot_tree(&backup, &backup)?;
    clear_app_directory_for_restore(&app_dir)?;
    for entry in std::fs::read_dir(&backup)
        .map_err(|error| AppError::Io(format!("read {}: {error}", backup.display())))?
    {
        let entry = entry
            .map_err(|error| AppError::Io(format!("read {} entry: {error}", backup.display())))?;
        copy_tree_entry(&entry.path(), &app_dir.join(entry.file_name()), &backup)?;
    }
    Ok(())
}

fn validate_snapshot_tree(path: &Path, tree_root: &Path) -> Result<(), AppError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        AppError::StorageCorrupt(format!("inspect {}: {error}", path.display()))
    })?;
    if metadata.file_type().is_symlink() {
        // Same rule as `copy_tree_entry`, and it has to be: a validator
        // stricter than the writer would refuse to restore a backup this
        // module had just written itself.
        contained_symlink_target(path, tree_root)?;
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).map_err(|error| {
            AppError::StorageCorrupt(format!("read {}: {error}", path.display()))
        })? {
            let entry = entry.map_err(|error| {
                AppError::StorageCorrupt(format!("read {} entry: {error}", path.display()))
            })?;
            validate_snapshot_tree(&entry.path(), tree_root)?;
        }
    } else if !metadata.is_file() {
        return Err(AppError::StorageCorrupt(format!(
            "scaffold recovery snapshot contains a special file {}",
            path.display()
        )));
    }
    Ok(())
}

fn read_scaffold_recovery_journal(
    root: &Path,
    app_id: &str,
) -> Result<Option<ScaffoldRecoveryJournal>, AppError> {
    let rel = scaffold_recovery_journal_rel(app_id);
    let body = match rooted_fs::read_to_string_limited(root, &rel, MAX_DOC_BYTES) {
        Ok(body) => body,
        Err(FsError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(load_read_error(&rel, &error)),
    };
    let journal: ScaffoldRecoveryJournal = serde_json::from_str(&body)
        .map_err(|error| AppError::StorageCorrupt(format!("{}: {error}", rel.display())))?;
    if journal.schema_version != APPS_SCHEMA_VERSION {
        return Err(AppError::StorageCorrupt(format!(
            "{}: unsupported schemaVersion {} (expected {APPS_SCHEMA_VERSION})",
            rel.display(),
            journal.schema_version
        )));
    }
    if journal.app_id != app_id || !ids::is_valid_app_id(&journal.app_id) {
        return Err(AppError::StorageCorrupt(format!(
            "{}: journal app id {:?} does not match {:?}",
            rel.display(),
            journal.app_id,
            app_id
        )));
    }
    if journal.target_name.trim().is_empty() || journal.target_brief.trim().is_empty() {
        return Err(AppError::StorageCorrupt(format!(
            "{}: scaffold target identity must not be empty",
            rel.display()
        )));
    }
    let Some(suffix) = journal
        .backup_name
        .strip_prefix(&format!("{}-", journal.app_id))
    else {
        return Err(AppError::StorageCorrupt(format!(
            "{}: backup is not bound to app {}",
            rel.display(),
            app_id
        )));
    };
    if suffix.is_empty()
        || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        || journal.backup_name.contains('/')
        || journal.backup_name.contains('\\')
    {
        return Err(AppError::StorageCorrupt(format!(
            "{}: invalid scaffold recovery backup name {:?}",
            rel.display(),
            journal.backup_name
        )));
    }
    Ok(Some(journal))
}

fn write_scaffold_recovery_journal(
    root: &Path,
    journal: &ScaffoldRecoveryJournal,
) -> Result<(), AppError> {
    let mut body = serde_json::to_vec_pretty(journal)
        .map_err(|error| AppError::Io(format!("serialize scaffold recovery journal: {error}")))?;
    body.push(b'\n');
    if body.len() as u64 > MAX_DOC_BYTES {
        return Err(AppError::InvalidRequest(
            "scaffold recovery journal exceeds the durable size limit".into(),
        ));
    }
    rooted_fs::atomic_write(
        root,
        &scaffold_recovery_journal_rel(&journal.app_id),
        &body,
        AtomicWriteOptions::default(),
    )
    .map_err(|error| AppError::from_fs("write scaffold recovery journal", &error))
}

fn remove_scaffold_recovery_journal(root: &Path, app_id: &str) -> Result<(), AppError> {
    match rooted_fs::remove_file(root, &scaffold_recovery_journal_rel(app_id)) {
        Ok(()) => Ok(()),
        Err(FsError::NotFound(_)) => Ok(()),
        Err(error) => Err(AppError::from_fs(
            "remove scaffold recovery journal",
            &error,
        )),
    }
}

fn scaffold_commit_is_durable(
    root: &Path,
    journal: &ScaffoldRecoveryJournal,
    index_record: Option<&AppRecord>,
) -> Result<bool, AppError> {
    let target_matches = |record: &AppRecord| {
        record.scaffolded
            && record.name == journal.target_name
            && record.brief == journal.target_brief
    };
    if index_record.is_some_and(target_matches) {
        return Ok(true);
    }
    let rel = metadata_rel(&journal.app_id);
    let body = match rooted_fs::read_to_string_limited(root, &rel, MAX_DOC_BYTES) {
        Ok(body) => body,
        Err(FsError::NotFound(_)) => return Ok(false),
        Err(error) => return Err(load_read_error(&rel, &error)),
    };
    let mirror: AppMetadataFile = serde_json::from_str(&body)
        .map_err(|error| AppError::StorageCorrupt(format!("{}: {error}", rel.display())))?;
    ensure_schema_version(&rel, mirror.schema_version)?;
    if mirror.app.id != journal.app_id {
        return Err(AppError::StorageCorrupt(format!(
            "{} mirrors app id {:?} but journal belongs to {:?}",
            rel.display(),
            mirror.app.id,
            journal.app_id
        )));
    }
    ensure_workspace_rel(&rel.display().to_string(), &mirror.app)?;
    if mirror.app.scaffolded
        && (mirror.app.name != journal.target_name || mirror.app.brief != journal.target_brief)
    {
        return Err(AppError::StorageCorrupt(format!(
            "{} has a scaffolded record that does not match the recovery target",
            rel.display()
        )));
    }
    Ok(target_matches(&mirror.app))
}

fn recover_scaffold_transaction_locked(
    root: &Path,
    app_id: &str,
    index_record: Option<&AppRecord>,
) -> Result<(), AppError> {
    let Some(journal) = read_scaffold_recovery_journal(root, app_id)? else {
        return Ok(());
    };
    let backup = rooted_fs::checked_join(root, &scaffold_recovery_backup_rel(&journal.backup_name))
        .map_err(|error| AppError::from_fs("resolve scaffold recovery backup", &error))?;
    if scaffold_commit_is_durable(root, &journal, index_record)? {
        // Remove the backup first while the journal remains. If cleanup is
        // interrupted, the next load sees the committed mirror and retries
        // both the missing backup and journal without rolling back.
        if let Err(error) = remove_owned_path(&backup) {
            tracing::warn!(app_id, path = %backup.display(), error = %error, "committed scaffold backup cleanup deferred");
            return Ok(());
        }
        if let Err(error) = remove_scaffold_recovery_journal(root, app_id) {
            tracing::warn!(app_id, error = %error, "committed scaffold journal cleanup deferred");
        }
        return Ok(());
    }
    restore_app_snapshot(root, &journal)?;
    // Keep the journal until the restored shell is complete. If this remove
    // fails, the shell is still authoritative and a later load safely repeats
    // the idempotent restore from the immutable backup.
    remove_scaffold_recovery_journal(root, app_id)?;
    remove_owned_path(&backup)?;
    Ok(())
}

/// Start a first-scaffold transaction while the caller holds the app build
/// lock. Existing journals are recovered first, which makes an in-process
/// retry after a failed landing as safe as a cold-start retry.
pub fn begin_scaffold_recovery(
    root: &Path,
    app_id: &str,
    target_name: &str,
    target_brief: &str,
) -> Result<ScaffoldRecoveryHandle, AppError> {
    ids::validate_app_id(app_id)?;
    if target_name.trim().is_empty() || target_brief.trim().is_empty() {
        return Err(AppError::InvalidRequest(
            "scaffold recovery target identity must not be empty".into(),
        ));
    }
    // A previous failed attempt may have been interrupted after the caller
    // released its receipt but before its synchronous rollback completed.
    // Recover it under the already-held build lock before taking a fresh
    // snapshot. No index hint is needed: the mirror is the service commit
    // point and carries the target identity recorded in the journal.
    if read_scaffold_recovery_journal(root, app_id)?.is_some() {
        recover_scaffold_transaction_locked(root, app_id, None)?;
    }
    let app_dir = rooted_fs::checked_join(root, &app_dir_rel(app_id))
        .map_err(|error| AppError::from_fs("begin scaffold recovery", &error))?;
    let app_metadata = std::fs::symlink_metadata(&app_dir).map_err(|error| {
        AppError::Io(format!(
            "inspect scaffold app {}: {error}",
            app_dir.display()
        ))
    })?;
    if !app_metadata.is_dir() || app_metadata.file_type().is_symlink() {
        return Err(AppError::StorageCorrupt(format!(
            "scaffold app {} is not a real directory",
            app_dir.display()
        )));
    }
    let recovery_root = rooted_fs::checked_join(root, &scaffold_recovery_dir_rel())
        .map_err(|error| AppError::from_fs("begin scaffold recovery", &error))?;
    private_dir(&recovery_root)?;
    let mut backup_name = None;
    let mut backup = None;
    for _ in 0..16 {
        let candidate = format!("{}-{}", app_id, ids::generate_app_id());
        let candidate_path = recovery_root.join(&candidate);
        match std::fs::symlink_metadata(&candidate_path) {
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                backup_name = Some(candidate);
                backup = Some(candidate_path);
                break;
            }
            Err(error) => {
                return Err(AppError::Io(format!(
                    "inspect scaffold backup {}: {error}",
                    candidate_path.display()
                )))
            }
        }
    }
    let backup_name = backup_name.ok_or_else(|| {
        AppError::Io("could not allocate a unique scaffold recovery backup".into())
    })?;
    let backup = backup.expect("backup path allocated with backup name");
    if let Err(error) = copy_app_snapshot(&app_dir, &backup) {
        let _ = remove_owned_path(&backup);
        return Err(error);
    }
    let journal = ScaffoldRecoveryJournal {
        schema_version: APPS_SCHEMA_VERSION,
        app_id: app_id.to_string(),
        backup_name: backup_name.clone(),
        target_name: target_name.trim().to_string(),
        target_brief: target_brief.trim().to_string(),
    };
    if let Err(error) = write_scaffold_recovery_journal(root, &journal) {
        let _ = remove_owned_path(&backup);
        return Err(error);
    }
    Ok(ScaffoldRecoveryHandle {
        root: root.to_path_buf(),
        app_id: app_id.to_string(),
        backup_name,
    })
}

impl ScaffoldRecoveryHandle {
    /// Mark the scaffold committed and remove its rollback material. The
    /// backup is removed before the journal so an interrupted cleanup can be
    /// retried by load-time recovery without rolling back a committed app.
    pub fn commit(self) -> Result<(), AppError> {
        let backup =
            rooted_fs::checked_join(&self.root, &scaffold_recovery_backup_rel(&self.backup_name))
                .map_err(|error| AppError::from_fs("resolve scaffold recovery backup", &error))?;
        remove_owned_path(&backup)?;
        remove_scaffold_recovery_journal(&self.root, &self.app_id)?;
        Ok(())
    }

    /// Restore the exact shell snapshot and remove its journal. If cleanup
    /// fails the journal remains, so a retry or cold start can repeat recovery.
    pub fn rollback(self) -> Result<(), AppError> {
        let journal_path = scaffold_recovery_journal_rel(&self.app_id);
        let journal =
            read_scaffold_recovery_journal(&self.root, &self.app_id)?.ok_or_else(|| {
                AppError::StorageCorrupt("scaffold recovery journal disappeared".into())
            })?;
        restore_app_snapshot(&self.root, &journal)?;
        let backup =
            rooted_fs::checked_join(&self.root, &scaffold_recovery_backup_rel(&self.backup_name))
                .map_err(|error| AppError::from_fs("resolve scaffold recovery backup", &error))?;
        // Remove the JOURNAL first, matching this file's own recovery-path
        // rollback (`recover_scaffold_transaction_locked`). `commit`'s
        // opposite (backup-first) order is safe only because a repeat lands
        // in the `scaffold_commit_is_durable == true` branch, which never
        // restores; rollback has no such branch. Removing the backup first
        // here means a `remove_dir_all` that fails PART WAY leaves a
        // TRUNCATED backup still named by a live journal, and the next
        // `load_all` would wipe the app we just restored correctly and copy
        // that partial tree over it — `validate_snapshot_tree` only rejects
        // escaping symlinks and special files, so nothing catches it. A
        // backup left behind by a failed
        // removal is merely unreachable disk, reclaimed by
        // `sweep_orphaned_scaffold_backups` once the app is deleted; the
        // error is propagated (not warned) so the caller still learns.
        remove_scaffold_recovery_journal(&self.root, &self.app_id).map_err(|error| {
            AppError::Io(format!(
                "remove scaffold recovery journal {}: {error}",
                journal_path.display()
            ))
        })?;
        remove_owned_path(&backup)?;
        Ok(())
    }
}

/// Serialize a document the way the repo persists native-feature JSON:
/// pretty 2-space indent plus a single trailing newline.
fn serialize_doc<T: Serialize>(rel: &Path, value: &T) -> Result<String, AppError> {
    let mut body = serde_json::to_string_pretty(value)
        .map_err(|error| AppError::Io(format!("serialize {}: {error}", rel.display())))?;
    body.push('\n');
    Ok(body)
}

fn write_doc<T: Serialize>(root: &Path, rel: &Path, value: &T) -> Result<(), AppError> {
    let body = serialize_doc(rel, value)?;
    // Enforce the load-side size contract at the write seam, BEFORE any temp
    // file is created: a document the loader would refuse must never reach
    // disk, where it would brick the whole store at the next load. The
    // mutation fails typed instead, and the caller's memory rollback keeps
    // memory == disk.
    let within = u64::try_from(body.len()).is_ok_and(|len| len <= MAX_DOC_BYTES);
    if !within {
        return Err(AppError::InvalidRequest(format!(
            "document {} would exceed the durable size limit: {} bytes (limit {MAX_DOC_BYTES})",
            rel.display(),
            body.len()
        )));
    }
    rooted_fs::atomic_write(root, rel, body.as_bytes(), AtomicWriteOptions::default())
        .map_err(|error| write_error(rel, &error))
}

/// Map a write failure. Every path this module writes is derived from a
/// pre-validated app id (never a raw caller path), so a containment violation
/// here — a symlink or DIRECTORY squatting on a document's final path, which
/// `atomic_write` refuses before renaming — is store tampering, not caller
/// error: it maps to `storage_corrupt`, mirroring [`load_read_error`]'s
/// treatment of the same squat on the read side. Everything else stays `Io`.
fn write_error(rel: &Path, error: &FsError) -> AppError {
    match error {
        FsError::OutsideWorkspace(_) => AppError::StorageCorrupt(format!(
            "{} is squatted by a non-regular file (symlink or directory on a \
             document path); refusing to write through it",
            rel.display()
        )),
        other => AppError::from_fs(&format!("write {}", rel.display()), other),
    }
}

/// Map a read failure in the LOAD path. Tampering with the store's documents
/// is storage corruption, not caller error: a missing file, an
/// over-[`MAX_DOC_BYTES`] body, non-UTF-8 bytes, and a symlink (or other
/// non-regular file) squatting on a document path are all `storage_corrupt`
/// — for a *listed* app every document must exist as a well-formed regular
/// file within the size contract. `invalid_request` stays reserved for
/// genuinely caller-supplied bad paths outside the load path (see
/// [`AppError::from_fs`]); genuine I/O failures stay `Io`.
fn load_read_error(rel: &Path, error: &FsError) -> AppError {
    match error {
        FsError::NotFound(_) => AppError::StorageCorrupt(format!("{} is missing", rel.display())),
        FsError::TooLarge { actual, limit } => AppError::StorageCorrupt(format!(
            "{} is {actual} bytes (limit {limit})",
            rel.display()
        )),
        FsError::BinaryFile(_) => {
            AppError::StorageCorrupt(format!("{} is not valid UTF-8", rel.display()))
        }
        FsError::OutsideWorkspace(_) => AppError::StorageCorrupt(format!(
            "{} is not a regular file contained in the store (symlink or special file \
             squatting on a document path)",
            rel.display()
        )),
        other => AppError::from_fs(&format!("read {}", rel.display()), other),
    }
}

/// Read + strictly parse one schema-versioned document; every out-of-contract
/// shape fails typed `storage_corrupt` (see [`load_read_error`]).
fn read_doc<T: DeserializeOwned>(root: &Path, rel: &Path) -> Result<T, AppError> {
    let body = rooted_fs::read_to_string_limited(root, rel, MAX_DOC_BYTES)
        .map_err(|error| load_read_error(rel, &error))?;
    serde_json::from_str(&body)
        .map_err(|error| AppError::StorageCorrupt(format!("{}: {error}", rel.display())))
}

fn ensure_schema_version(rel: &Path, found: u32) -> Result<(), AppError> {
    if found == APPS_SCHEMA_VERSION {
        Ok(())
    } else if found < APPS_SCHEMA_VERSION {
        Err(AppError::StorageCorrupt(format!(
            "{}: unsupported schemaVersion {found} (expected {APPS_SCHEMA_VERSION}); \
             this pre-release store is no longer supported, so clear the app dev data \
             (delete apps/) and create again / 此预发布 schema 的 store 已不再支持，\
             请清除应用开发数据（删除 apps/）后重新创建",
            rel.display()
        )))
    } else {
        Err(AppError::StorageCorrupt(format!(
            "{}: unsupported schemaVersion {found} (expected {APPS_SCHEMA_VERSION})",
            rel.display()
        )))
    }
}

/// Load every app from disk. A missing index means an empty store; a corrupt
/// index or per-app document fails with `storage_corrupt` rather than
/// silently dropping apps.
///
/// A store torn by a crash between the per-app batch and the index rewrite is
/// detected here (mirror/index divergence — see the module doc) and repaired
/// forward, and a runtime record stranded busy by a crash is reconciled (no
/// runtime process outlives the engine — see [`reconcile_runtime_at_load`]);
/// both are persisted before returning.
///
/// Legacy pipeline documents (`interactions.json`, `design-spec.json`) are
/// NOT read — stale files from old stores are simply ignored on disk.
pub fn load_all(root: &Path) -> Result<Vec<AppState>, AppError> {
    // The whole load — initial read, torn-commit repairs, and their index
    // rewrite — is one read-modify-write transaction under the advisory
    // index lock (finding 9), so a concurrent instance's save cannot
    // interleave with the repair writes. Everything below writes the index
    // through the RAW `save_index` (this transaction already holds the lock;
    // re-acquiring would self-deadlock). The lock file lives inside the
    // store, so an absent root/`apps/` skeleton is created first (a fresh,
    // empty store).
    std::fs::create_dir_all(root.join(APPS_DIR))
        .map_err(|error| AppError::Io(format!("create {}: {error}", root.display())))?;
    // Recovery is acquired BEFORE `index.lock`: an active scaffold host owns
    // this lock before its app `build.lock`, and its commit then takes the
    // index lock. Keeping the same order here prevents the loader from
    // deadlocking with a scaffold that is waiting to publish its record.
    let _recovery_lock = lock_scaffold_recovery(root)?;
    let _lock = lock_index(root)?;
    sweep_trash(root);
    sweep_orphaned_scaffold_backups(root);
    let index_rel = index_rel();
    let body = match rooted_fs::read_to_string_limited(root, &index_rel, MAX_DOC_BYTES) {
        Ok(body) => body,
        Err(FsError::NotFound(_)) => {
            // No index at all, so nothing on disk is committed: every marked
            // `apps/<id>` here is by definition a create that never reached
            // its commit point. Sweeping BEFORE the early return is the whole
            // point — this is exactly the store shape a crash on the very
            // first create leaves behind, and returning early without it was
            // how such a store leaked forever.
            sweep_uncommitted_app_dirs(root, &BTreeSet::new());
            return Ok(Vec::new());
        }
        Err(error) => return Err(load_read_error(&index_rel, &error)),
    };
    let index: AppIndexFile = serde_json::from_str(&body).map_err(|error| {
        // A template-era `apps/index.json` fails to parse (it lacks the
        // now-required `brief` field) like any other shape drift — but unlike
        // an ordinary corruption, this ISN'T a bug to report, it's an
        // intentionally unreadable legacy format: give a message that says so
        // instead of leaking the raw serde path. The check is on the raw
        // bytes, not the (already-failed) typed value, so it fires
        // regardless of where in the document the old `"template"` field
        // happened to sit.
        if body.as_bytes().windows(10).any(|w| w == b"\"template\"") {
            AppError::StorageCorrupt(
                "此版本不再支持模版时代的 app 记录（apps/index.json 含 template 字段）；\
                 请删除 apps/ 目录后重新创建应用 / no longer supports template-era app records"
                    .into(),
            )
        } else if error.to_string().contains("missing field `scaffolded`") {
            // 规格 §A.2：`AppRecord.scaffolded` 故意没有 `#[serde(default)]`，
            // 缺字段必须加载失败——静默变成 `false` 就是一个 shell，而 shell 是
            // `LocalAppScaffold` 会清空的状态，等于销毁用户真实的源码。失败保住了
            // 数据，但裸的 serde `missing field` 不会告诉读到它的人该做什么：能看到
            // 这条消息的只有手里还留着本分支之前的 store 的开发者，正确动作是清除
            // 该应用的开发数据。所以这里和模版时代那一支一样，把解析失败翻译成可执行
            // 的指引，而不是泄漏 serde 路径。
            AppError::StorageCorrupt(
                "apps/index.json 的 app 记录缺少 scaffolded 字段：这个 store 早于\
                 对话式创建（conversational-create）改动，此版本不再支持；\
                 请清除本应用的开发数据（删除 apps/ 目录）后重新创建应用 / \
                 record without `scaffolded` predates the conversational-create \
                 change: clear the app's dev data (delete apps/) and create again"
                    .into(),
            )
        } else {
            AppError::StorageCorrupt(format!("apps/index.json: {error}"))
        }
    })?;
    ensure_schema_version(&index_rel, index.schema_version)?;
    // Reclaim create skeletons stranded by a crash between `save_app_files`
    // and `save_index_preserving`. It runs here rather than beside the other
    // two sweeps above because it is the only one that needs to know what the
    // index lists, and it runs BEFORE the per-record loop so a reclaimed id
    // is free again by the time this load returns.
    sweep_uncommitted_app_dirs(
        root,
        &index
            .apps
            .iter()
            .map(|record| record.id.clone())
            .collect::<BTreeSet<String>>(),
    );

    let mut apps = Vec::with_capacity(index.apps.len());
    let mut seen_ids = BTreeSet::new();
    let mut any_repaired = false;
    for record in index.apps {
        // A hostile id in a tampered index must never turn into a path.
        if !ids::is_valid_app_id(&record.id) {
            return Err(AppError::StorageCorrupt(format!(
                "apps/index.json lists invalid app id {:?}",
                record.id
            )));
        }
        // A duplicated id would alias one directory across two entries and
        // detonate later (deleting one strands the twin over a removed
        // directory); fail loudly at load time like an invalid id.
        if !seen_ids.insert(record.id.clone()) {
            return Err(AppError::StorageCorrupt(format!(
                "apps/index.json lists app id {:?} more than once",
                record.id
            )));
        }
        // Finding 7: `workspace_rel` carries the documented invariant
        // `apps/<id>/workspace` — validated for exact equality BEFORE any
        // per-app document is read, so a tampered value is rejected instead
        // of being laundered back into the index by a later repair rewrite.
        ensure_workspace_rel("apps/index.json", &record)?;

        // A first-scaffold landing is a host transaction whose record commit
        // happens after the manifest/workspace writes. Resolve any durable
        // journal before reading runtime, metadata, or dependency state so a
        // crash cannot route a half-landed app through the service.
        //
        // r2-failure-paths-08: this call takes NO per-app build lock of its
        // own — `load_all` already holds the broader index lock (`_lock`
        // above) for the whole read-modify-write transaction, which already
        // serializes a second engine instance against this recovery, so an
        // additional per-app lock here would be redundant. (A per-app build
        // lock IS required for a caller that recovers a single app WITHOUT
        // first taking the index lock, such as [`begin_scaffold_recovery`].)
        recover_scaffold_transaction_locked(root, &record.id, Some(&record))?;

        let runtime_rel = runtime_rel(&record.id);
        let runtime: AppRuntimeRecord = read_doc(root, &runtime_rel)?;
        ensure_schema_version(&runtime_rel, runtime.schema_version)?;
        // Finding 6: the embedded owner id must match the app the document
        // belongs to, like the mirror id below.
        if runtime.app_id != record.id {
            return Err(AppError::StorageCorrupt(format!(
                "{} claims app id {:?} but belongs to app {:?}",
                runtime_rel.display(),
                runtime.app_id,
                record.id
            )));
        }

        let metadata_rel = metadata_rel(&record.id);
        let mirror: AppMetadataFile = read_doc(root, &metadata_rel)?;
        ensure_schema_version(&metadata_rel, mirror.schema_version)?;
        if mirror.app.id != record.id {
            return Err(AppError::StorageCorrupt(format!(
                "{} mirrors app id {:?} but the index lists {:?}",
                metadata_rel.display(),
                mirror.app.id,
                record.id
            )));
        }
        // Finding 7 (mirror side): the mirror record can supersede the index
        // record in torn-commit repair, so its `workspace_rel` is validated
        // BEFORE repair may adopt (and re-persist) it.
        ensure_workspace_rel(&metadata_rel.display().to_string(), &mirror.app)?;

        let mut app = AppState { record, runtime };
        let repaired = repair_torn_commit(&mut app, &mirror.app);
        if repaired {
            tracing::warn!(
                app_id = %app.record.id,
                repaired_updated_at_ms = app.record.updated_at_ms,
                "local-apps store was torn by a crash mid-commit; repaired forward"
            );
        }
        let reconciled = reconcile_runtime_at_load(&mut app);
        if repaired {
            save_app_files(root, &app)?;
            any_repaired = true;
        } else if reconciled {
            save_runtime(root, &app.record.id, &app.runtime)?;
        }
        apps.push(app);
    }
    if any_repaired {
        let records: Vec<AppRecord> = apps.iter().map(|app| app.record.clone()).collect();
        save_index(root, &records)?;
    }
    Ok(apps)
}

/// Load one app's dependency record. Missing files are derived for backwards
/// compatibility and become durable the next time the host updates them.
pub fn load_dependency_record(
    root: &Path,
    record: &AppRecord,
) -> Result<AppDependencyRecord, AppError> {
    let dependency_rel = dependency_rel(&record.id);
    match rooted_fs::read_to_string_limited(root, &dependency_rel, MAX_DOC_BYTES) {
        Ok(body) => {
            let dependency: AppDependencyRecord = serde_json::from_str(&body).map_err(|error| {
                AppError::StorageCorrupt(format!("{}: {error}", dependency_rel.display()))
            })?;
            ensure_schema_version(&dependency_rel, dependency.schema_version)?;
            if dependency.app_id != record.id {
                return Err(AppError::StorageCorrupt(format!(
                    "{} claims app id {:?} but belongs to app {:?}",
                    dependency_rel.display(),
                    dependency.app_id,
                    record.id
                )));
            }
            Ok(dependency)
        }
        Err(FsError::NotFound(_)) => Ok(derived_dependency_record(root, record)),
        Err(error) => Err(load_read_error(&dependency_rel, &error)),
    }
}

/// Enforce the documented `workspace_rel` invariant — EXACT equality with
/// `apps/<id>/workspace` (finding 7). Applied to index records and the
/// `app.json` mirror at load, so repair can never launder a tampered value
/// back into the index: nothing is adopted or re-persisted unvalidated.
fn ensure_workspace_rel(source: &str, record: &AppRecord) -> Result<(), AppError> {
    let expected = workspace_rel_str(&record.id);
    if record.workspace_rel == expected {
        Ok(())
    } else {
        Err(AppError::StorageCorrupt(format!(
            "{source}: app {:?} workspaceRel {:?} violates the invariant {expected:?}",
            record.id, record.workspace_rel
        )))
    }
}

/// Reconcile a runtime record stranded busy by a crash. No runtime process
/// outlives the engine, so at load time nothing can genuinely still be
/// starting/running/stopping: `stopping` settles to `stopped` (the shutdown
/// it was waiting for cannot outlive the process), and `starting`/`running`
/// become `failed` with a `last_error` explaining the reconciliation —
/// otherwise the record would claim a live runtime forever and e.g.
/// `delete_app` would refuse with `runtime_busy` with no path out. Returns
/// `true` when the record changed (the caller persists).
///
/// ⚠️ GATE (the twin of the warning on
/// `AppService::update_runtime_record`): this unconditional stranding policy
/// is CORRECT ONLY while no runtime process can outlive the engine. A future
/// live process manager MUST replace it with a liveness-aware
/// reconciliation, or loads will stamp genuinely running dev servers
/// `failed` and un-guard deletion.
fn reconcile_runtime_at_load(app: &mut AppState) -> bool {
    let reconciled_state = match app.runtime.state {
        AppRuntimeState::Stopping => AppRuntimeState::Stopped,
        AppRuntimeState::Starting | AppRuntimeState::Running => AppRuntimeState::Failed,
        AppRuntimeState::Stopped | AppRuntimeState::Failed => return false,
    };
    tracing::warn!(
        app_id = %app.record.id,
        stranded_state = %app.runtime.state,
        reconciled_state = %reconciled_state,
        "runtime record was stranded busy by a crash; reconciled at load"
    );
    if reconciled_state == AppRuntimeState::Failed {
        app.runtime.last_error = Some("reconciled at load: no live runtime manager".to_string());
    }
    app.runtime.state = reconciled_state;
    true
}

/// Reconcile one loaded app against a crash torn between the per-app batch
/// and the index rewrite. Returns `true` when anything was repaired (the
/// caller persists).
///
/// Whole-batch tear: the `app.json` mirror is written LAST in
/// [`save_app_files`] and BEFORE the index, so a mirror/index divergence
/// proves the batch committed while the index write was lost. The mirror
/// record wins.
fn repair_torn_commit(app: &mut AppState, mirror: &AppRecord) -> bool {
    if *mirror != app.record {
        app.record = mirror.clone();
        return true;
    }
    false
}

/// Atomically replace `apps/index.json` with `records` — RAW, whole-index
/// authority, no lock of its own. Legal callers either already hold the
/// index lock for a wider transaction ([`load_all`]'s repair rewrite) or are
/// single-writer seeds (tests, fixtures). The service's cross-instance index
/// transactions go through [`save_index_preserving`] instead.
pub fn save_index(root: &Path, records: &[AppRecord]) -> Result<(), AppError> {
    let doc = AppIndexFile {
        schema_version: APPS_SCHEMA_VERSION,
        apps: records.to_vec(),
    };
    write_doc(root, &index_rel(), &doc)
}

/// One locked `apps/index.json` read-modify-write transaction (finding 9):
/// under the advisory index lock, re-read the disk index and write `records`
/// PLUS every disk entry whose id is in neither `records` nor `known_ids` —
/// an app created by a foreign process/instance this writer has never seen
/// (preserved verbatim, with a warning). For ids the writer HAS seen
/// (`known_ids` = every id it ever loaded, created or deleted) it stays
/// authoritative: an id it deleted is absent from `records` and is NOT
/// resurrected from disk.
///
/// A disk index that exists but cannot be read/parsed is logged and treated
/// as empty — this write then repairs it (foreign entries in the unreadable
/// document are unrecoverable either way).
pub fn save_index_preserving(
    root: &Path,
    records: &[AppRecord],
    known_ids: &BTreeSet<String>,
) -> Result<(), AppError> {
    let _lock = lock_index(root)?;
    let disk: Vec<AppRecord> =
        match rooted_fs::read_to_string_limited(root, &index_rel(), MAX_DOC_BYTES) {
            Ok(body) => match serde_json::from_str::<AppIndexFile>(&body) {
                Ok(index) => index.apps,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "apps/index.json on disk is unparseable during a locked index write; \
                         rewriting it from this instance's records"
                    );
                    Vec::new()
                }
            },
            Err(FsError::NotFound(_)) => Vec::new(),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "apps/index.json on disk is unreadable during a locked index write; \
                     rewriting it from this instance's records"
                );
                Vec::new()
            }
        };
    let mut merged = records.to_vec();
    let mut merged_ids: BTreeSet<String> = merged.iter().map(|record| record.id.clone()).collect();
    for entry in disk {
        if !known_ids.contains(&entry.id) && merged_ids.insert(entry.id.clone()) {
            tracing::warn!(
                app_id = %entry.id,
                "preserving a foreign-process app index entry this instance has never seen"
            );
            merged.push(entry);
        }
    }
    save_index(root, &merged)
}

/// One per-app persisted document, as a step of [`APP_DOC_WRITE_ORDER`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppDocWriteStep {
    /// `apps/<id>/runtime.json` ([`save_runtime`]).
    Runtime,
    /// `workspace/.lingxi/app.json` record mirror — always LAST;
    /// [`load_all`]'s torn-commit repair depends on it superseding the index.
    MetadataMirror,
}

/// THE canonical per-app write order, defined exactly once.
/// [`save_app_files`] executes this sequence in full; partial writers (the
/// service's changed-docs mutation path) must pass subsequences of it to
/// [`save_app_files_steps`] — the repair contract in [`load_all`] depends on
/// the ORDER of the writes that happen, not on every document being
/// rewritten.
pub const APP_DOC_WRITE_ORDER: [AppDocWriteStep; 2] =
    [AppDocWriteStep::Runtime, AppDocWriteStep::MetadataMirror];

/// The true prefix of [`APP_DOC_WRITE_ORDER`] up to and including `last` —
/// what a crash inside [`save_app_files`] leaves behind (a mid-batch crash
/// always commits a prefix of the canonical sequence, never a reordering).
/// Crash tests replay these instead of hand-picking an assumed order.
#[must_use]
pub fn write_order_prefix_through(last: AppDocWriteStep) -> &'static [AppDocWriteStep] {
    let position = APP_DOC_WRITE_ORDER
        .iter()
        .position(|step| *step == last)
        .expect("every step appears in APP_DOC_WRITE_ORDER");
    &APP_DOC_WRITE_ORDER[..=position]
}

/// Failure from [`save_app_files_steps`]: the underlying error plus how far
/// the batch got. Each step is one atomic write, so the batch always fails
/// BETWEEN steps: the first `written` steps of the requested slice hold their
/// NEW documents on disk, the failing step and everything after it are
/// untouched. Callers use `written` to compensate precisely (the service's
/// mid-batch rollback rewrites exactly the succeeded prefix's originals —
/// finding on `with_app`).
#[derive(Debug)]
pub struct AppBatchWriteFailure {
    /// How many leading steps of the requested slice were durably written
    /// before the failure (their NEW documents are on disk).
    pub written: usize,
    /// The failing step's error.
    pub error: AppError,
}

impl AppBatchWriteFailure {
    /// The failing step's error, discarding the progress report.
    #[must_use]
    pub fn into_error(self) -> AppError {
        self.error
    }
}

/// Atomically persist the given per-app documents for `app`, in the given
/// order. [`save_app_files`] passes the full [`APP_DOC_WRITE_ORDER`]; the
/// service's mutation path passes the subsequence of steps whose documents
/// actually changed. Does NOT touch the index — callers write the index last
/// (the creation commit point; for mutations a lost index write is repaired
/// forward on load, see the module doc). On failure the error reports WHICH
/// prefix of `steps` already landed (see [`AppBatchWriteFailure`]).
pub fn save_app_files_steps(
    root: &Path,
    app: &AppState,
    steps: &[AppDocWriteStep],
) -> Result<(), AppBatchWriteFailure> {
    let id = &app.record.id;
    for (written, step) in steps.iter().enumerate() {
        let result = match step {
            AppDocWriteStep::Runtime => save_runtime(root, id, &app.runtime),
            AppDocWriteStep::MetadataMirror => {
                let mirror = AppMetadataFile {
                    schema_version: APPS_SCHEMA_VERSION,
                    app: app.record.clone(),
                };
                write_doc(root, &metadata_rel(id), &mirror)
            }
        };
        if let Err(error) = result {
            return Err(AppBatchWriteFailure { written, error });
        }
    }
    Ok(())
}

/// Atomically persist every per-app document in [`APP_DOC_WRITE_ORDER`]
/// (runtime, then the metadata mirror — the mirror LAST).
pub fn save_app_files(root: &Path, app: &AppState) -> Result<(), AppError> {
    save_app_files_steps(root, app, &APP_DOC_WRITE_ORDER).map_err(AppBatchWriteFailure::into_error)
}

/// Atomically persist `apps/<id>/runtime.json`.
pub fn save_runtime(root: &Path, app_id: &str, runtime: &AppRuntimeRecord) -> Result<(), AppError> {
    write_doc(root, &runtime_rel(app_id), runtime)
}

/// Atomically persist `apps/<id>/dependencies.json`.
pub fn save_dependency_record(
    root: &Path,
    dependency: &AppDependencyRecord,
) -> Result<(), AppError> {
    write_doc(root, &dependency_rel(&dependency.app_id), dependency)
}

/// Remove `apps/<id>`: serialize against any in-flight build or background
/// task, then
/// rename-to-trash first (the commit point — see
/// [`trash_app_dir`]), then recursively remove the trash entry. A removal
/// failure AFTER the rename still leaves the id fully out of the `apps/`
/// namespace; the leftover trash entry is swept at the next load.
pub fn delete_app_dir(root: &Path, app_id: &str) -> Result<(), AppError> {
    let result = match trash_app_dir(root, app_id)? {
        None => Ok(()),
        Some(trash_path) => std::fs::remove_dir_all(&trash_path)
            .map_err(|error| AppError::Io(format!("remove {}: {error}", trash_path.display()))),
    };
    // r1-backlog-scaffold-build-03 / r1-failure-paths-008: create staging for
    // this app (`.lingxi-build-state/template-candidates/<app_id>/…`) hangs
    // off the apps DATA ROOT, a sibling of `apps/` — so trashing `apps/<id>`
    // above never touches it, and nothing else ever reclaims it once the app
    // is gone. Best-effort only: it is diagnostic litter at this point, never
    // load-bearing, and must not turn a successful app deletion into an
    // error.
    let template_candidates = root
        .join(".lingxi-build-state")
        .join("template-candidates")
        .join(app_id);
    if let Err(error) = std::fs::symlink_metadata(&template_candidates) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                app_id,
                path = %template_candidates.display(),
                error = %error,
                "stat create-staging leftovers during app deletion"
            );
        }
    } else if let Err(error) = std::fs::remove_dir_all(&template_candidates) {
        tracing::warn!(
            app_id,
            path = %template_candidates.display(),
            error = %error,
            "create-staging leftovers cleanup deferred"
        );
    }
    result
}

/// COMMIT POINT of app-directory deletion: atomically rename `apps/<id>` into
/// `apps/.trash/<id>-<8-hex nonce>` and return the trash path (`None` when
/// there was nothing to move — the dir is absent, or a squatting
/// symlink/file was removed as the link itself, never followed).
///
/// Why rename instead of `remove_dir_all` in place (TOCTOU finding):
/// `remove_dir_all` re-resolves every intermediate component on each step of
/// its traversal, while `rename` never follows the FINAL component of either
/// path — one atomic metadata operation moves the whole tree out of the
/// `apps/` namespace. The rename is also the tombstone: `mint_app_id`
/// re-checks disk (live dir OR trash entry), so a racing create can never
/// adopt a directory a stale removal is still tearing down.
///
/// The id is re-validated (grammar forbids separators and `..`), the join is
/// lexically checked, and `apps/`, `apps/.trash` and `apps/<id>` must be
/// real directories.
pub fn trash_app_dir(root: &Path, app_id: &str) -> Result<Option<PathBuf>, AppError> {
    ids::validate_app_id(app_id)?;
    // Keep the lock through the rename commit point. Once the directory is in
    // `.trash`, no build can resolve it through the live app path anymore and
    // the lock can be released safely before the best-effort recursive remove.
    let _background_lock = lock_app_background_if_present(root, app_id)?;
    let _build_lock = lock_app_build_if_present(root, app_id)?;
    let apps_rel = PathBuf::from(APPS_DIR);
    let apps_dir = rooted_fs::checked_join(root, &apps_rel)
        .map_err(|error| AppError::from_fs("delete app dir", &error))?;
    match std::fs::symlink_metadata(&apps_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(AppError::Io(format!(
                "stat {}: {error}",
                apps_dir.display()
            )));
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(AppError::StorageCorrupt(format!(
                "{} is not a real directory",
                apps_dir.display()
            )));
        }
        Ok(_) => {}
    }

    let target = rooted_fs::checked_join(root, &app_dir_rel(app_id))
        .map_err(|error| AppError::from_fs("delete app dir", &error))?;
    match std::fs::symlink_metadata(&target) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::Io(format!("stat {}: {error}", target.display()))),
        Ok(meta) if meta.is_dir() => {
            let trash_dir = apps_dir.join(TRASH_DIR);
            std::fs::create_dir_all(&trash_dir).map_err(|error| {
                AppError::Io(format!("create {}: {error}", trash_dir.display()))
            })?;
            // A file or symlink squatting on `.trash` must never receive the
            // rename (a symlink would be followed by the create/rename above
            // it); insist on a real directory like `apps/` itself.
            match std::fs::symlink_metadata(&trash_dir) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => {
                    return Err(AppError::StorageCorrupt(format!(
                        "{} is not a real directory",
                        trash_dir.display()
                    )));
                }
                Err(error) => {
                    return Err(AppError::Io(format!(
                        "stat {}: {error}",
                        trash_dir.display()
                    )));
                }
            }
            let trash_path = trash_dir.join(format!("{app_id}-{}", ids::generate_app_id()));
            std::fs::rename(&target, &trash_path).map_err(|error| {
                AppError::Io(format!(
                    "rename {} -> {}: {error}",
                    target.display(),
                    trash_path.display()
                ))
            })?;
            Ok(Some(trash_path))
        }
        // A symlink (or stray file) squatting on the app path is removed as
        // the link itself; its target is never followed.
        Ok(_) => {
            std::fs::remove_file(&target)
                .map_err(|error| AppError::Io(format!("remove {}: {error}", target.display())))?;
            Ok(None)
        }
    }
}

/// True when `app_id` is still present ON DISK — a live `apps/<id>` entry (of
/// any file type) or a `.trash/<id>-<nonce>` tombstone. `mint_app_id`
/// consults this in addition to the in-memory list so a freshly-deleted (or
/// orphaned) directory can never be adopted by a same-id create racing a
/// stale removal. Read failures conservatively report "absent" (minting must
/// not wedge on an unreadable trash dir; the collision odds of a random
/// 8-hex id are negligible).
#[must_use]
pub fn app_id_present_on_disk(root: &Path, app_id: &str) -> bool {
    if !ids::is_valid_app_id(app_id) {
        return false;
    }
    let Ok(dir) = rooted_fs::checked_join(root, &app_dir_rel(app_id)) else {
        return false;
    };
    if std::fs::symlink_metadata(&dir).is_ok() {
        return true;
    }
    let Ok(trash) = rooted_fs::checked_join(root, &trash_dir_rel()) else {
        return false;
    };
    let prefix = format!("{app_id}-");
    match std::fs::read_dir(&trash) {
        Ok(entries) => entries
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(&prefix)),
        Err(_) => false,
    }
}

/// Split a scaffold-recovery backup's directory name into the app id that
/// owns it. `begin_scaffold_recovery` mints the name as
/// `format!("{app_id}-{}", ids::generate_app_id())`, and `generate_app_id`
/// always yields exactly 8 lowercase-hex characters — so the suffix is
/// unambiguous even though `app_id` itself may contain hyphens. Returns
/// `None` for anything that does not match that shape, so a stray or
/// hand-placed entry is left untouched rather than guessed at.
fn parse_scaffold_backup_owner(backup_name: &str) -> Option<String> {
    if backup_name.len() < 10 {
        return None;
    }
    // `backup_name` is an arbitrary on-disk entry name, not necessarily one
    // this code minted: `str::split_at` PANICS when the index is not a UTF-8
    // char boundary, and this runs inside `load_all` ahead of the index read,
    // so a single stray CJK/emoji-named directory here would take the whole
    // profile load down. Fail the match instead, as the doc above promises.
    let cut = backup_name.len() - 8;
    if !backup_name.is_char_boundary(cut) {
        return None;
    }
    let (prefix, suffix) = backup_name.split_at(cut);
    if !suffix
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    prefix.strip_suffix('-').map(str::to_string)
}

/// How long an uncommitted `apps/<id>` skeleton is left alone before
/// [`sweep_uncommitted_app_dirs`] reclaims it.
///
/// The AGE, not a lock, is what tells an in-flight create apart from a crash
/// leftover. The creator holds no lock between [`save_app_files`] and
/// [`save_index_preserving`], so there is nothing a sweeper could wait on that
/// would prove the skeleton dead; and `rooted_fs` — the only writer this
/// module goes through, because it is the one that keeps every path inside the
/// store root — exposes just the BLOCKING `rooted_fs::lock_exclusive`, with no
/// try-lock. (A non-blocking `File::try_lock_exclusive` DOES exist in this
/// workspace, `platform-api/src/live_sessions.rs`, but it is std's, taken on a
/// raw `File` outside the rooted-path guard; adopting it here would mean a new
/// `rooted_fs` primitive, not a one-line swap. It is a real option if this
/// design is ever revisited — it is not an absent one.) Age settles it
/// instead: an in-flight create is seconds old, and no create survives a
/// process restart, so anything still marked a full day later is definitively
/// a crash leftover.
///
/// The trade is stated plainly: a create that genuinely spanned more than a
/// day between its skeleton and its index commit would be reclaimed by
/// another instance's load. Every initializer on this path is a single
/// short workspace write (`prepare_shell_app` / `write_guided_contract_value`),
/// so that window is not reachable in practice.
///
/// LOCK ORDER — read this before shortening the constant or reusing the sweep.
/// The sweep takes no lock while it DECIDES, but reclamation runs through
/// [`delete_app_dir`] -> [`trash_app_dir`], which DOES take the app's
/// background and build locks, blocking, while [`load_all`] is still holding
/// the scaffold-recovery lock and the index lock. That is the inverse of the
/// order a creating or scaffolding host takes them in (per-app lock first, the
/// index lock at its commit), so on paper it is an ABBA pair. What makes it
/// unreachable is this constant: to deadlock, some thread would have to be
/// holding the build lock of an app that is ABSENT from the index and waiting
/// on the index lock, for a skeleton whose marker is already a full day old —
/// i.e. a create in flight for 24 hours. Both of the sweep's other conditions
/// (absent from `indexed`, marker older than this) are load-bearing for that
/// argument, not just for the data. If a shorter grace is ever wanted, drop
/// the per-app locks on this path first;
/// [`recover_scaffold_transaction_locked`] is the precedent for how — it takes
/// NO per-app lock precisely because `load_all` already holds the index lock.
const UNCOMMITTED_CREATE_GRACE_MS: u64 = 24 * 60 * 60 * 1000;

/// Mark `apps/<id>` as an in-flight, not-yet-committed create.
///
/// Called BEFORE the first byte of the skeleton is written; cleared by
/// [`clear_app_creating`] once `apps/index.json` lists the app. Between those
/// two points a crash strands the directory, and the marker is the only thing
/// that later identifies it as reclaimable.
pub fn mark_app_creating(root: &Path, app_id: &str) -> Result<(), AppError> {
    ids::validate_app_id(app_id)?;
    let rel = creating_marker_rel(app_id);
    // Contents are diagnostic only — the sweep reads the marker's mtime, not
    // its body — but a human staring at a leaked directory deserves to be
    // told what the file is.
    rooted_fs::atomic_write(
        root,
        &rel,
        b"local-apps: this app's create never reached its index commit\n",
        AtomicWriteOptions::default(),
    )
    .map_err(|error| write_error(&rel, &error))
}

/// Clear the in-flight-create marker for `apps/<id>`.
///
/// Best-effort by contract: it runs AFTER the index commit that publishes the
/// app, so a failure here cannot un-create anything. A marker left behind is
/// swept by the next [`load_all`] (an indexed app's marker is removed, never
/// its directory), which is why the caller only logs.
pub fn clear_app_creating(root: &Path, app_id: &str) -> Result<(), AppError> {
    ids::validate_app_id(app_id)?;
    let path = rooted_fs::checked_join(root, &creating_marker_rel(app_id))
        .map_err(|error| AppError::from_fs("clear create marker", &error))?;
    remove_owned_path(&path)
}

/// Best-effort sweep of `apps/<id>` directories whose create never reached
/// its commit point (r1-failure-paths-007).
///
/// `save_app_files` + `AppLayout::initialize` build the skeleton, and
/// `save_index_preserving` is the commit; a crash in between leaves a
/// directory that index-driven enumeration can never see again — and, because
/// [`app_id_present_on_disk`] treats any live directory as a collision, the
/// id it occupies is burned for good. Nothing else in this module reclaims
/// it: `sweep_trash` reads `apps/.trash` only, and
/// `sweep_orphaned_scaffold_backups` reads `apps/.scaffold-recovery` only.
///
/// The sweep reclaims a directory only when ALL of these hold, and is
/// deliberately conservative on every read failure:
///   1. its name is a valid app id (so `index.json`, `index.lock`, `.trash`
///      and `.scaffold-recovery` are never candidates);
///   2. it is a real directory, not a symlink or file squatting the name;
///   3. it carries a [`CREATING_MARKER_FILE`] — i.e. a create started here and
///      never finished;
///   4. `indexed` does not list it (an indexed app is a real app; a stale
///      marker on one is removed, and the directory is left alone);
///   5. the marker is older than [`UNCOMMITTED_CREATE_GRACE_MS`], which is
///      what keeps a CONCURRENT instance's in-flight create safe without the
///      sweep taking a lock to decide.
///
/// Reclamation goes through [`delete_app_dir`], the audited removal primitive:
/// it renames into `apps/.trash` first (never following the final component)
/// and also drops the app's `.lingxi-build-state/template-candidates/<id>`
/// staging, which is the same litter an uncommitted create leaves behind.
/// That primitive was audited for callers who hold NO index lock, and it
/// blocks on the app's background and build locks — see the LOCK ORDER
/// paragraph on [`UNCOMMITTED_CREATE_GRACE_MS`] for why that is safe HERE,
/// under `load_all`'s index lock, and for what would break it.
fn sweep_uncommitted_app_dirs(root: &Path, indexed: &BTreeSet<String>) {
    let Ok(apps_dir) = rooted_fs::checked_join(root, &PathBuf::from(APPS_DIR)) else {
        return;
    };
    let entries = match std::fs::read_dir(&apps_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "apps/ is unreadable; uncommitted-create sweep skipped"
            );
            return;
        }
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !ids::is_valid_app_id(&name) {
            continue;
        }
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => {}
            _ => continue,
        }
        let Ok(marker) = rooted_fs::checked_join(root, &creating_marker_rel(&name)) else {
            continue;
        };
        let Ok(marker_meta) = std::fs::symlink_metadata(&marker) else {
            // No marker: either a committed app or a directory this code did
            // not create. Not ours to reclaim.
            continue;
        };
        if indexed.contains(&name) {
            // The create DID commit and only the marker removal was lost.
            // Reclaim the file, never the app.
            if let Err(error) = remove_owned_path(&marker) {
                tracing::warn!(
                    app_id = %name,
                    error = %error,
                    "stale create marker on a committed app could not be removed"
                );
            }
            continue;
        }
        let fresh = marker_meta
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_none_or(|age| age.as_millis() < u128::from(UNCOMMITTED_CREATE_GRACE_MS));
        if fresh {
            // Either genuinely young (a create in flight in another instance,
            // right now) or a clock that cannot be read/compared — both mean
            // "leave it alone".
            continue;
        }
        tracing::warn!(
            app_id = %name,
            "reclaiming an app directory whose create never reached its index commit"
        );
        if let Err(error) = delete_app_dir(root, &name) {
            tracing::warn!(
                app_id = %name,
                error = %error,
                "uncommitted app directory could not be reclaimed; leaving it for the next load"
            );
        }
    }
}

/// Best-effort sweep of `apps/.scaffold-recovery` (r1-engine-core-005).
///
/// A first-scaffold backup's journal lives INSIDE the app directory it
/// backs up (`apps/<id>/<journal file>`), while the backup itself lives
/// outside it, in this sibling directory. Deleting the app therefore
/// deletes the journal but never this backup, and — because
/// `recover_scaffold_transaction_locked` above only ever runs for an app id
/// still present in the index — nothing else ever revisits it again. Reclaim
/// any backup whose owning app directory no longer exists. Failures are
/// logged and NEVER fail the load.
fn sweep_orphaned_scaffold_backups(root: &Path) {
    let Ok(recovery_dir) = rooted_fs::checked_join(root, &scaffold_recovery_dir_rel()) else {
        return;
    };
    let entries = match std::fs::read_dir(&recovery_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "apps/.scaffold-recovery is unreadable; sweep skipped"
            );
            return;
        }
    };
    for entry in entries.flatten() {
        let backup_name = entry.file_name().to_string_lossy().to_string();
        let Some(app_id) = parse_scaffold_backup_owner(&backup_name) else {
            continue;
        };
        if !ids::is_valid_app_id(&app_id) {
            continue;
        }
        let Ok(app_dir) = rooted_fs::checked_join(root, &app_dir_rel(&app_id)) else {
            continue;
        };
        if std::fs::symlink_metadata(&app_dir).is_ok() {
            // The app is still live; its own journal governs this backup,
            // not this sweep.
            continue;
        }
        let path = entry.path();
        if let Err(error) = remove_owned_path(&path) {
            tracing::warn!(
                app_id = %app_id,
                path = %path.display(),
                error = %error,
                "orphaned scaffold recovery backup could not be swept; leaving it for the next load"
            );
        }
    }
}

/// Best-effort sweep of `apps/.trash` (leftovers from removals that failed
/// or were interrupted after their commit-point rename). Failures are logged
/// and NEVER fail the load.
fn sweep_trash(root: &Path) {
    let Ok(trash) = rooted_fs::checked_join(root, &trash_dir_rel()) else {
        return;
    };
    let entries = match std::fs::read_dir(&trash) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(error = %error, "apps/.trash is unreadable; sweep skipped");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let removal = match entry.file_type() {
            Ok(kind) if kind.is_dir() => std::fs::remove_dir_all(&path),
            _ => std::fs::remove_file(&path),
        };
        if let Err(error) = removal {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "trash entry could not be swept; leaving it for the next load"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AppErrorCode;
    fn new_app(id: &str) -> AppState {
        AppState::create(
            id.into(),
            format!("App {id}"),
            "a test app".into(),
            Some("conv-9".into()),
            1_700_000_000_000,
        )
    }

    fn save_full(root: &Path, apps: &[AppState]) {
        for app in apps {
            save_app_files(root, app).unwrap();
        }
        let records: Vec<_> = apps.iter().map(|a| a.record.clone()).collect();
        save_index(root, &records).unwrap();
    }

    /// Build the exact on-disk shape a crash between `save_app_files` and
    /// `save_index_preserving` leaves behind: a complete `apps/<id>` skeleton
    /// that `apps/index.json` does not list, still carrying its in-flight
    /// create marker.
    fn seed_uncommitted_skeleton(root: &Path, id: &str) {
        let app = new_app(id);
        mark_app_creating(root, id).unwrap();
        save_app_files(root, &app).unwrap();
    }

    /// Push the create marker's mtime `age_ms` into the past. The sweep reads
    /// the marker's modification time, so this is what lets a test cross the
    /// grace period without sleeping through it.
    fn backdate_creating_marker(root: &Path, id: &str, age_ms: u64) {
        let path = root.join(APPS_DIR).join(id).join(CREATING_MARKER_FILE);
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_millis(age_ms))
            .unwrap();
    }

    #[test]
    fn an_in_flight_create_skeleton_is_left_alone_inside_the_grace_period() {
        // The concurrency case the sweep must never get wrong: another engine
        // instance is between its skeleton and its index commit RIGHT NOW.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let committed = new_app("keeper01");
        save_full(root, std::slice::from_ref(&committed));
        seed_uncommitted_skeleton(root, "inflight1");

        let loaded = load_all(root).unwrap();
        assert_eq!(loaded.len(), 1, "the in-flight app is not published");
        assert!(
            root.join(APPS_DIR).join("inflight1").is_dir(),
            "a seconds-old create skeleton must survive another instance's load"
        );
        assert!(app_id_present_on_disk(root, "inflight1"));
    }

    #[test]
    fn an_aged_uncommitted_create_skeleton_is_reclaimed_and_frees_its_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let committed = new_app("keeper01");
        save_full(root, std::slice::from_ref(&committed));
        seed_uncommitted_skeleton(root, "stranded1");
        backdate_creating_marker(root, "stranded1", UNCOMMITTED_CREATE_GRACE_MS + 60_000);
        assert!(app_id_present_on_disk(root, "stranded1"));

        let loaded = load_all(root).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].record.id, "keeper01");
        assert!(
            !root.join(APPS_DIR).join("stranded1").exists(),
            "a crash-stranded skeleton must be reclaimed"
        );
        assert!(
            !app_id_present_on_disk(root, "stranded1"),
            "reclaiming must also free the id the skeleton was burning"
        );
        assert!(
            root.join(APPS_DIR).join("keeper01").is_dir(),
            "a committed app is untouched by the sweep"
        );
    }

    #[test]
    fn an_aged_uncommitted_skeleton_is_reclaimed_even_when_no_index_was_ever_written() {
        // The crash-on-the-very-first-create store: `apps/index.json` does not
        // exist at all, which used to return early before any sweep ran.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        seed_uncommitted_skeleton(root, "firstever");
        backdate_creating_marker(root, "firstever", UNCOMMITTED_CREATE_GRACE_MS + 60_000);
        assert!(!root.join(APPS_DIR).join(INDEX_FILE).exists());

        assert!(load_all(root).unwrap().is_empty());
        assert!(
            !root.join(APPS_DIR).join("firstever").exists(),
            "an indexless store must still be swept"
        );
    }

    #[test]
    fn a_committed_app_loses_a_stale_create_marker_but_keeps_its_directory() {
        // A crash between the index commit and the marker removal.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let app = new_app("survivor");
        save_full(root, std::slice::from_ref(&app));
        mark_app_creating(root, "survivor").unwrap();
        backdate_creating_marker(root, "survivor", UNCOMMITTED_CREATE_GRACE_MS + 60_000);

        let loaded = load_all(root).unwrap();
        assert_eq!(loaded.len(), 1, "the app itself must survive");
        assert!(root.join(APPS_DIR).join("survivor").is_dir());
        assert!(
            !root
                .join(APPS_DIR)
                .join("survivor")
                .join(CREATING_MARKER_FILE)
                .exists(),
            "the stale marker on a committed app must be reclaimed"
        );
    }

    #[test]
    fn an_unmarked_unindexed_directory_is_never_reclaimed() {
        // Without a marker the sweep has no evidence the directory came from
        // an unfinished create, so it must keep its hands off however old it
        // is — deleting on "absent from the index" alone is how a live app
        // gets destroyed.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        save_full(root, &[]);
        let unmarked = new_app("unmarked1");
        save_app_files(root, &unmarked).unwrap();

        assert!(load_all(root).unwrap().is_empty());
        assert!(
            root.join(APPS_DIR).join("unmarked1").is_dir(),
            "an unmarked directory must never be swept"
        );
    }

    #[test]
    fn load_recovers_a_partial_first_scaffold_before_reading_app_documents() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppState::create(
            "scaffold1".into(),
            "untitled".into(),
            "original brief".into(),
            None,
            1_700_000_000_000,
        );
        app.record.scaffolded = false;
        save_full(dir.path(), std::slice::from_ref(&app));
        let layout = crate::manifest::AppLayout::new(dir.path(), app.record.id.clone()).unwrap();
        layout.initialize().unwrap();
        let manifest = crate::manifest::AppManifest::for_new_app("scaffold1", "untitled");
        crate::manifest::save_manifest(&layout, &manifest).unwrap();
        let workspace = dir.path().join(layout.workspace_rel());
        std::fs::write(workspace.join("LINGXI.md"), "guided shell\n").unwrap();

        let lock = lock_app_build(dir.path(), "scaffold1").unwrap();
        let recovery =
            begin_scaffold_recovery(dir.path(), "scaffold1", "formed name", "formed brief")
                .unwrap();
        let mut partial = manifest.clone();
        partial.name = "formed name".into();
        partial.surface = Some(crate::manifest::AppSurface::Canvas);
        partial.runtime_profile = Some(crate::manifest::AppRuntimeProfileBinding {
            family: crate::types::AppRuntimeProfile::Canvas2d,
            revision: 1,
            contract_sha256: "0".repeat(64),
        });
        partial.template_origin = Some(crate::manifest::AppTemplateOrigin {
            plugin_id: crate::manifest::AppTemplateOrigin::BUILTIN_PLUGIN_ID.into(),
            plugin_version: "builtin".into(),
            template_id: "canvas-2d-r1".into(),
            template_sha256: "0".repeat(64),
        });
        crate::manifest::save_manifest(&layout, &partial).unwrap();
        std::fs::write(workspace.join("premature.js"), "discard me\n").unwrap();
        // A process crash drops the handle without running rollback.
        std::mem::forget(recovery);
        drop(lock);

        let loaded = load_all(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(!loaded[0].record.scaffolded);
        assert_eq!(
            crate::manifest::load_manifest(&layout).unwrap(),
            manifest,
            "load-time recovery must restore the pre-scaffold manifest"
        );
        assert_eq!(
            std::fs::read(workspace.join("LINGXI.md")).unwrap(),
            b"guided shell\n"
        );
        assert!(!workspace.join("premature.js").exists());
        assert!(!dir
            .path()
            .join(scaffold_recovery_journal_rel("scaffold1"))
            .exists());
    }

    /// Build the "+" button's shell on disk: index row, layout, manifest and
    /// the guided `workspace/LINGXI.md`. Returns the workspace path.
    #[cfg(unix)]
    fn shell_on_disk(root: &Path, app_id: &str) -> PathBuf {
        let mut app = AppState::create(
            app_id.into(),
            "untitled".into(),
            "original brief".into(),
            None,
            1_700_000_000_000,
        );
        app.record.scaffolded = false;
        save_full(root, std::slice::from_ref(&app));
        let layout = crate::manifest::AppLayout::new(root, app.record.id.clone()).unwrap();
        layout.initialize().unwrap();
        let manifest = crate::manifest::AppManifest::for_new_app(app_id, "untitled");
        crate::manifest::save_manifest(&layout, &manifest).unwrap();
        let workspace = root.join(layout.workspace_rel());
        std::fs::write(workspace.join("LINGXI.md"), "guided shell\n").unwrap();
        workspace
    }

    /// r1-backlog-scaffold-build-09. The recovery snapshot is taken BEFORE the
    /// wipe that would have removed the workspace's links, so refusing every
    /// symlink outright made a shell containing ANY link impossible to
    /// scaffold — ever, on any retry. A link that stays inside the tree must
    /// survive the snapshot/rollback round trip AS A LINK.
    #[test]
    #[cfg(unix)]
    fn scaffold_recovery_round_trips_a_contained_relative_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = shell_on_disk(dir.path(), "symlink1");
        std::fs::create_dir_all(workspace.join("bin")).unwrap();
        std::os::unix::fs::symlink("../LINGXI.md", workspace.join("bin/contract.md")).unwrap();

        let lock = lock_app_build(dir.path(), "symlink1").unwrap();
        let recovery =
            begin_scaffold_recovery(dir.path(), "symlink1", "formed name", "formed brief")
                .expect("a contained symlink must not block the first scaffold");
        // What a landing scaffold does: wipe the editable surface, write the
        // formal contract.
        std::fs::remove_dir_all(workspace.join("bin")).unwrap();
        std::fs::write(workspace.join("LINGXI.md"), "formal contract\n").unwrap();
        recovery.rollback().expect("rollback");
        drop(lock);

        let link = workspace.join("bin/contract.md");
        let metadata = std::fs::symlink_metadata(&link)
            .expect("the rolled-back workspace must have the link back");
        assert!(
            metadata.file_type().is_symlink(),
            "restored as a {:?}, not a symlink",
            metadata.file_type()
        );
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("../LINGXI.md"),
            "the link must be reproduced, not dereferenced into a copy"
        );
        assert_eq!(
            std::fs::read(workspace.join("LINGXI.md")).unwrap(),
            b"guided shell\n",
            "and the rest of the rollback must still work"
        );
    }

    /// The other half: containment is the rule, not "symlinks are fine now".
    #[test]
    #[cfg(unix)]
    fn scaffold_recovery_refuses_an_escaping_or_absolute_symlink() {
        for (label, target, needle) in [
            ("escaping", "../../../../etc/passwd", "escapes the snapshot"),
            ("absolute", "/etc/passwd", "must be relative"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let workspace = shell_on_disk(dir.path(), "symlink2");
            std::os::unix::fs::symlink(target, workspace.join("stowaway")).unwrap();

            let lock = lock_app_build(dir.path(), "symlink2").unwrap();
            let error = begin_scaffold_recovery(dir.path(), "symlink2", "formed", "brief")
                .expect_err("{label}: a link out of the tree must fail closed");
            drop(lock);
            assert!(
                matches!(&error, AppError::StorageCorrupt(_)),
                "{label}: {error:?}"
            );
            assert!(
                error.to_string().contains(needle),
                "{label}: expected {needle:?}, got {error}"
            );
            assert!(
                !dir.path()
                    .join(scaffold_recovery_dir_rel())
                    .join("symlink2")
                    .exists(),
                "{label}: the refused snapshot must not leave a backup behind"
            );
        }
    }

    #[test]
    fn load_all_sweeps_a_scaffold_backup_whose_app_directory_was_deleted() {
        // r1-engine-core-005: a first-scaffold backup's journal lives INSIDE
        // the app directory it backs up, while the backup lives outside it
        // under `apps/.scaffold-recovery/`. Deleting the app takes the
        // journal with it but leaves the backup behind, and nothing besides
        // this sweep ever revisits a backup for an app id no longer present.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/orphan1")).unwrap();
        std::fs::write(dir.path().join("apps/orphan1/marker.txt"), b"x").unwrap();
        let lock = lock_app_build(dir.path(), "orphan1").unwrap();
        let recovery = begin_scaffold_recovery(dir.path(), "orphan1", "formed", "brief").unwrap();
        // A crash drops the handle without commit/rollback, exactly like the
        // partial-scaffold recovery test above.
        std::mem::forget(recovery);
        drop(lock);

        let recovery_dir = dir.path().join(scaffold_recovery_dir_rel());
        let backups_before = std::fs::read_dir(&recovery_dir).unwrap().count();
        assert_eq!(
            backups_before, 1,
            "begin_scaffold_recovery must have written exactly one backup"
        );

        // The app is deleted out from under the backup — its journal (which
        // lives inside `apps/orphan1/`) goes with it, exactly like a real
        // `delete_app_dir`.
        std::fs::remove_dir_all(dir.path().join("apps/orphan1")).unwrap();

        load_all(dir.path()).unwrap();

        assert_eq!(
            std::fs::read_dir(&recovery_dir).unwrap().count(),
            0,
            "load_all must sweep a scaffold-recovery backup whose owning app directory is gone"
        );
    }

    #[test]
    fn load_all_survives_a_non_ascii_entry_in_the_scaffold_recovery_dir() {
        // `sweep_orphaned_scaffold_backups` runs inside `load_all` BEFORE the
        // index is read, and feeds every directory entry name straight to
        // `parse_scaffold_backup_owner`. `str::split_at` panics when the cut
        // is not a UTF-8 char boundary, so a single stray CJK-named entry
        // used to take the whole profile load down instead of being skipped
        // (the two sweep tests above only ever use ASCII ids).
        let dir = tempfile::tempdir().unwrap();
        let recovery_dir = dir.path().join(scaffold_recovery_dir_rel());
        std::fs::create_dir_all(&recovery_dir).unwrap();
        // 18 bytes; byte index 10 (len - 8) lands inside the third character.
        let stray = "测试目录名字";
        assert!(
            !stray.is_char_boundary(stray.len() - 8),
            "this fixture only exercises the bug if len - 8 is mid-character"
        );
        std::fs::create_dir_all(recovery_dir.join(stray)).unwrap();

        let loaded = load_all(dir.path()).unwrap();
        assert_eq!(loaded.len(), 0, "empty profile still loads");
        assert!(
            recovery_dir.join(stray).exists(),
            "an entry that does not match the backup name shape must be left untouched"
        );
    }

    #[test]
    fn load_all_keeps_a_scaffold_backup_whose_app_is_still_live() {
        // The mirror image of the sweep test above: a backup for an app that
        // still exists (mid in-flight scaffold, journal present) must survive
        // the sweep untouched — only the per-record recovery path may act on
        // it, and here that recovers (rolls back) the pending scaffold, so
        // only the RECORD is what changes; the sweep itself must not have
        // deleted the backup out from under that recovery.
        let dir = tempfile::tempdir().unwrap();
        let mut app = AppState::create(
            "live1".into(),
            "untitled".into(),
            "original brief".into(),
            None,
            1_700_000_000_000,
        );
        app.record.scaffolded = false;
        save_full(dir.path(), std::slice::from_ref(&app));
        let layout = crate::manifest::AppLayout::new(dir.path(), app.record.id.clone()).unwrap();
        layout.initialize().unwrap();
        let manifest = crate::manifest::AppManifest::for_new_app("live1", "untitled");
        crate::manifest::save_manifest(&layout, &manifest).unwrap();

        let lock = lock_app_build(dir.path(), "live1").unwrap();
        let recovery = begin_scaffold_recovery(dir.path(), "live1", "formed", "brief").unwrap();
        std::mem::forget(recovery);
        drop(lock);

        let loaded = load_all(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1, "the live app must still load");
        assert!(
            !dir.path()
                .join(scaffold_recovery_journal_rel("live1"))
                .exists(),
            "the per-record recovery path (not the sweep) must have resolved this journal"
        );
    }

    #[test]
    fn round_trips_full_store() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = new_app("aaaa1111");
        app.set_runtime(
            AppRuntimeState::Starting,
            Some(3010),
            Some(9),
            None,
            1_700_000_000_001,
        )
        .unwrap();
        app.set_runtime(
            AppRuntimeState::Failed,
            None,
            None,
            Some("boot failed".into()),
            1_700_000_000_002,
        )
        .unwrap();
        let apps = vec![app, new_app("bbbb2222")];
        save_full(dir.path(), &apps);

        let loaded = load_all(dir.path()).unwrap();
        assert_eq!(loaded, apps);
        // Files live where the spec says.
        assert!(dir.path().join("apps/index.json").is_file());
        assert!(dir.path().join("apps/aaaa1111/runtime.json").is_file());
        assert!(dir
            .path()
            .join("apps/aaaa1111/workspace/.lingxi/app.json")
            .is_file());
        // Persisted documents are pretty-printed with a trailing newline.
        let body = std::fs::read_to_string(dir.path().join("apps/index.json")).unwrap();
        assert!(body.starts_with("{\n"));
        assert!(body.ends_with("}\n"));
        assert!(body.contains(&format!("\"schemaVersion\": {APPS_SCHEMA_VERSION}")));
    }

    /// The mirror-wins half of torn-commit repair: a crash after the per-app
    /// batch but before the index rewrite leaves the mirror AHEAD of the
    /// index — the mirror record supersedes the index record and the repair
    /// is persisted (both index and mirror agree on the next load).
    #[test]
    fn diverged_mirror_supersedes_the_index_and_the_repair_persists() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = new_app("cccc3333");
        save_full(dir.path(), std::slice::from_ref(&app));
        // A mutation committed its batch (runtime + mirror)…
        app.record.name = "Updated".into();
        app.record.updated_at_ms = 1_700_000_000_500;
        save_app_files(dir.path(), &app).unwrap();
        // …but the index rewrite was lost to a crash: index still says old.

        let loaded = load_all(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].record.name, "Updated", "the mirror wins");
        assert_eq!(loaded[0].record.updated_at_ms, 1_700_000_000_500);
        // The repair was persisted: the index now agrees.
        let body = std::fs::read_to_string(dir.path().join("apps/index.json")).unwrap();
        assert!(body.contains("\"Updated\""), "{body}");
        // A second load needs no repair and sees the same state.
        assert_eq!(load_all(dir.path()).unwrap(), loaded);
    }

    /// Legacy pipeline documents sitting in the app dir are ignored (not read,
    /// not deleted), and stale `workflowState` bytes in the index/mirror are
    /// tolerated as unknown fields rather than participating in state.
    #[test]
    fn stale_workflow_state_fields_and_legacy_docs_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let app = new_app("dddd4444");
        save_full(dir.path(), std::slice::from_ref(&app));
        // Rewrite index + mirror with a stale legacy workflowState field.
        for rel in [
            "apps/index.json",
            "apps/dddd4444/workspace/.lingxi/app.json",
        ] {
            let path = dir.path().join(rel);
            let body = std::fs::read_to_string(&path).unwrap();
            std::fs::write(
                &path,
                body.replace("\"draft\"", "\"awaiting_preview_confirmation\""),
            )
            .unwrap();
        }
        // Plant stale legacy pipeline documents.
        let stale_interactions = dir.path().join("apps/dddd4444/interactions.json");
        std::fs::write(&stale_interactions, "{\"schemaVersion\":1,\"nextSeq\":1}").unwrap();
        let stale_spec = dir
            .path()
            .join("apps/dddd4444/workspace/.lingxi/design-spec.json");
        std::fs::write(&stale_spec, "{\"schemaVersion\":1,\"revision\":3}").unwrap();

        let loaded = load_all(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].record, app.record);
        // The stale documents were left alone.
        assert_eq!(
            std::fs::read_to_string(&stale_interactions).unwrap(),
            "{\"schemaVersion\":1,\"nextSeq\":1}"
        );
        assert_eq!(
            std::fs::read_to_string(&stale_spec).unwrap(),
            "{\"schemaVersion\":1,\"revision\":3}"
        );
    }

    #[test]
    fn missing_index_is_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_all(dir.path()).unwrap(), Vec::new());
        // Root without even the apps/ dir is fine too.
        assert_eq!(
            load_all(&dir.path().join("nested-missing")).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn corrupt_index_is_storage_corrupt_not_silent_reset() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps")).unwrap();
        std::fs::write(dir.path().join("apps/index.json"), "{ not json").unwrap();
        let err = load_all(dir.path()).unwrap_err();
        assert_eq!(err.code(), AppErrorCode::StorageCorrupt);
    }

    #[test]
    fn a_template_era_index_reports_a_readable_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps")).unwrap();
        std::fs::write(
            dir.path().join("apps/index.json"),
            br#"{"schemaVersion":1,"apps":[{"id":"old","name":"Old","template":"dashboard","createdAtMs":1,"updatedAtMs":1,"workflowState":"ready","workspaceRel":"apps/old/workspace"}]}"#,
        )
        .unwrap();

        let error = load_all(dir.path()).expect_err("a template-era index is not loadable");
        let message = format!("{error}");
        assert!(
            message.contains("不再支持") || message.contains("no longer supports"),
            "the error explains WHY rather than leaking a serde path: {message}"
        );
    }

    /// 规格 §A.2：缺 `scaffolded` 的记录必须加载失败**并且**告诉开发者清除开发
    /// 数据。失败那一半由 `AppRecord` 没有 `#[serde(default)]` 保证（见
    /// `tests/serde_compat.rs`）；这里钉的是另一半——消息本身可执行。
    ///
    /// 断言故意不止 `contains("scaffolded")`：裸的 serde `missing field
    /// `scaffolded`` 也含这个词，那样的断言在没有本指引时照样绿。所以断言落在
    /// 指引文本上，并反向断言消息里不再出现 serde 的 `missing field` 路径。
    #[test]
    fn a_record_without_scaffolded_tells_the_developer_to_clear_dev_data() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps")).unwrap();
        // Every other required field is present under its real wire spelling,
        // so `scaffolded` is the only one serde can report missing. No
        // `"template"` byte anywhere, so the template-era arm cannot claim it.
        std::fs::write(
            dir.path().join("apps/index.json"),
            br#"{"schemaVersion":1,"apps":[{"id":"aaaa1111","name":"Legacy","brief":"b","createdAtMs":1,"updatedAtMs":1,"workflowState":"draft","workspaceRel":"apps/aaaa1111/workspace"}]}"#,
        )
        .unwrap();

        let error = load_all(dir.path()).expect_err("a record without `scaffolded` must not load");
        assert_eq!(error.code(), AppErrorCode::StorageCorrupt);
        let message = error.to_string();
        assert!(
            message.contains("scaffolded"),
            "the message names the field: {message}"
        );
        assert!(
            message.contains("conversational-create") && message.contains("对话式创建"),
            "the message says the store predates the conversational-create change: {message}"
        );
        assert!(
            message.contains("清除") && message.contains("delete apps/"),
            "the message tells the developer to clear the dev data: {message}"
        );
        assert!(
            !message.contains("missing field"),
            "the raw serde path is replaced by the guidance, not appended to it: {message}"
        );
    }

    #[test]
    fn unsupported_schema_version_is_storage_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps")).unwrap();
        std::fs::write(
            dir.path().join("apps/index.json"),
            "{\"schemaVersion\": 99, \"apps\": []}\n",
        )
        .unwrap();
        let err = load_all(dir.path()).unwrap_err();
        assert_eq!(err.code(), AppErrorCode::StorageCorrupt);
    }

    #[test]
    fn hostile_app_id_in_index_never_becomes_a_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps")).unwrap();
        let body = serde_json::json!({
            "schemaVersion": APPS_SCHEMA_VERSION,
            "apps": [{
                "id": "../../escape",
                "name": "evil",
                "brief": "an evil app",
                "scaffolded": true,
                "createdAtMs": 1,
                "updatedAtMs": 1,
                "workflowState": "draft",
                "workspaceRel": "apps/../../escape/workspace"
            }]
        });
        std::fs::write(dir.path().join("apps/index.json"), body.to_string()).unwrap();
        let err = load_all(dir.path()).unwrap_err();
        assert_eq!(err.code(), AppErrorCode::StorageCorrupt);
        // Pin the MESSAGE too, not just the error code: an unrelated
        // deserialize failure (e.g. a missing required field) also comes
        // back as `StorageCorrupt` and would let this test pass vacuously
        // without ever reaching the `is_valid_app_id` guard it exists to
        // pin (storage.rs's `apps/index.json lists invalid app id {:?}`).
        assert!(
            err.to_string().contains("invalid app id"),
            "rejection must come from the invalid-id guard, not an unrelated \
             deserialize failure: {err}"
        );
    }

    /// Finding 7: `workspace_rel` itself is validated against its documented
    /// invariant (`apps/<id>/workspace`, exactly) — a hostile value behind a
    /// perfectly VALID app id is rejected on THIS field, before any per-app
    /// document is read, so repair can never launder it back into the index.
    #[test]
    fn hostile_workspace_rel_with_valid_id_is_rejected_on_that_field() {
        for hostile in [
            "apps/../../escape/workspace",
            "apps/zzzz9999/workspace", // someone ELSE's workspace
            "workspace",
            "/etc",
            "apps/aaaa1111/workspace/", // trailing separator — not exact
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(dir.path().join("apps")).unwrap();
            let body = serde_json::json!({
                "schemaVersion": APPS_SCHEMA_VERSION,
                "apps": [{
                    "id": "aaaa1111",
                    "name": "sneaky",
                    "brief": "a sneaky app",
                    "scaffolded": true,
                    "createdAtMs": 1,
                    "updatedAtMs": 1,
                    "workflowState": "draft",
                    "workspaceRel": hostile
                }]
            });
            std::fs::write(dir.path().join("apps/index.json"), body.to_string()).unwrap();
            let err = load_all(dir.path()).unwrap_err();
            assert_eq!(err.code(), AppErrorCode::StorageCorrupt, "{hostile}");
            assert!(
                err.to_string().contains("workspaceRel"),
                "rejection must pin the workspaceRel field: {err}"
            );
        }
    }

    /// Finding 7 (mirror side): the `app.json` mirror can supersede the
    /// index record in repair, so its `workspace_rel` is validated BEFORE
    /// adoption — repair never persists a value it did not validate.
    #[test]
    fn tampered_mirror_workspace_rel_is_storage_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let app = new_app("qqqq7777");
        save_full(dir.path(), std::slice::from_ref(&app));
        let mirror_path = dir.path().join("apps/qqqq7777/workspace/.lingxi/app.json");
        let body = std::fs::read_to_string(&mirror_path).unwrap();
        std::fs::write(
            &mirror_path,
            body.replace("apps/qqqq7777/workspace", "apps/../../escape/workspace"),
        )
        .unwrap();
        let err = load_all(dir.path()).unwrap_err();
        assert_eq!(err.code(), AppErrorCode::StorageCorrupt, "{err}");
        assert!(err.to_string().contains("workspaceRel"), "{err}");
    }

    /// Finding 6: the runtime record's embedded `app_id` must name the
    /// owning app; a mismatch is `storage_corrupt` like the mirror id.
    #[test]
    fn embedded_app_id_mismatches_are_storage_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let app = new_app("ssss1111");
        save_full(dir.path(), std::slice::from_ref(&app));
        let mut runtime = app.runtime.clone();
        runtime.app_id = "tttt2222".into();
        save_runtime(dir.path(), "ssss1111", &runtime).unwrap();
        let err = load_all(dir.path()).unwrap_err();
        assert_eq!(err.code(), AppErrorCode::StorageCorrupt, "runtime: {err}");
        assert!(err.to_string().contains("claims app id"), "{err}");
    }

    /// Finding 10: the rename into `apps/.trash` is the deletion's commit
    /// point — after it, the id is fully out of the `apps/` namespace (a
    /// same-id create gets a FRESH directory; nothing is adopted) while the
    /// content awaits removal under the trash path, and the tombstone still
    /// blocks re-minting the id.
    #[test]
    fn trash_rename_commits_the_deletion_before_removal() {
        let dir = tempfile::tempdir().unwrap();
        let app = new_app("uuuu3333");
        save_full(dir.path(), std::slice::from_ref(&app));
        let marker = dir.path().join("apps/uuuu3333/workspace/user-data.txt");
        std::fs::write(&marker, b"precious").unwrap();

        let trash_path = trash_app_dir(dir.path(), "uuuu3333")
            .unwrap()
            .expect("a live dir moves to trash");
        // Committed: the live path is gone even though nothing was removed
        // yet; the content sits intact under the returned trash path.
        assert!(!dir.path().join("apps/uuuu3333").exists());
        assert!(trash_path.join("workspace/user-data.txt").is_file());
        // The tombstone still pins the id against re-minting…
        assert!(app_id_present_on_disk(dir.path(), "uuuu3333"));
        // …but a same-id create-after-delete writes a FRESH workspace with
        // no trace of the trashed content (no orphan adoption).
        let recreated = new_app("uuuu3333");
        save_app_files(dir.path(), &recreated).unwrap();
        assert!(!dir
            .path()
            .join("apps/uuuu3333/workspace/user-data.txt")
            .exists());
        assert!(dir
            .path()
            .join("apps/uuuu3333/workspace/.lingxi/app.json")
            .is_file());
        // The old content is still only in the trash, and the slow removal
        // targets the nonce'd trash path — never the recreated dir.
        std::fs::remove_dir_all(&trash_path).unwrap();
        assert!(dir
            .path()
            .join("apps/uuuu3333/workspace/.lingxi/app.json")
            .is_file());
    }

    /// Finding 10: `.trash` leftovers are swept (best-effort) at the next
    /// load, and the sweep never fails the load; `load_all` itself ignores
    /// `.trash` entirely (enumeration is index-driven).
    #[test]
    fn trash_leftovers_are_swept_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let keep = new_app("vvvv4444");
        let gone = new_app("wwww5555");
        save_app_files(dir.path(), &keep).unwrap();
        save_app_files(dir.path(), &gone).unwrap();
        save_index(dir.path(), &[keep.record.clone()]).unwrap();
        // A removal that renamed but never finished deleting.
        let trash_path = trash_app_dir(dir.path(), "wwww5555").unwrap().unwrap();
        assert!(trash_path.exists());
        assert!(app_id_present_on_disk(dir.path(), "wwww5555"));

        let loaded = load_all(dir.path()).unwrap();
        assert_eq!(
            loaded,
            vec![keep],
            ".trash must be invisible to enumeration"
        );
        assert!(!trash_path.exists(), "the leftover tombstone is swept");
        assert!(!app_id_present_on_disk(dir.path(), "wwww5555"));
    }

    /// Finding 10: `app_id_present_on_disk` truth table — live dir, trash
    /// tombstone, absent.
    #[test]
    fn app_id_disk_presence_covers_live_and_trash_entries() {
        let dir = tempfile::tempdir().unwrap();
        let app = new_app("xxxx6666");
        save_full(dir.path(), std::slice::from_ref(&app));
        assert!(app_id_present_on_disk(dir.path(), "xxxx6666"), "live dir");
        assert!(!app_id_present_on_disk(dir.path(), "yyyy7777"), "absent");
        let trash_path = trash_app_dir(dir.path(), "xxxx6666").unwrap().unwrap();
        assert!(
            app_id_present_on_disk(dir.path(), "xxxx6666"),
            "trash tombstone still pins the id"
        );
        // Another id sharing a PREFIX is not confused with the tombstone.
        assert!(!app_id_present_on_disk(dir.path(), "xxxx666"));
        std::fs::remove_dir_all(trash_path).unwrap();
        assert!(
            !app_id_present_on_disk(dir.path(), "xxxx6666"),
            "fully gone"
        );
    }

    /// Finding 9: `save_index_preserving` keeps foreign-process entries this
    /// writer has never seen, stays authoritative for known ids (deletions
    /// included), and never duplicates.
    #[test]
    fn save_index_preserving_merges_foreign_entries_and_honors_deletions() {
        let dir = tempfile::tempdir().unwrap();
        let ours = new_app("aaaa1111");
        let foreign = new_app("bbbb2222");
        let deleted = new_app("cccc3333");
        // Disk currently lists the foreign app and one we are deleting.
        save_index(
            dir.path(),
            &[foreign.record.clone(), deleted.record.clone()],
        )
        .unwrap();
        // We know about `ours` (writing it) and `deleted` (we deleted it);
        // the foreign entry is unknown to us and must survive.
        let known: BTreeSet<String> = [ours.record.id.clone(), deleted.record.id.clone()].into();
        save_index_preserving(dir.path(), std::slice::from_ref(&ours.record), &known).unwrap();

        let body = std::fs::read_to_string(dir.path().join("apps/index.json")).unwrap();
        let index: AppIndexFile = serde_json::from_str(&body).unwrap();
        let ids: Vec<&str> = index.apps.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["aaaa1111", "bbbb2222"],
            "ours first, foreign preserved, our deletion NOT resurrected"
        );
    }

    #[test]
    fn duplicate_app_id_in_index_is_storage_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let app = new_app("hhhh8888");
        save_full(dir.path(), std::slice::from_ref(&app));
        // A restored/merged backup (or hand edit) lists the same id twice.
        save_index(dir.path(), &[app.record.clone(), app.record.clone()]).unwrap();
        let err = load_all(dir.path()).unwrap_err();
        assert_eq!(err.code(), AppErrorCode::StorageCorrupt);
    }

    #[test]
    fn listed_app_with_missing_document_is_storage_corrupt() {
        for missing in [
            "apps/cccc3333/runtime.json",
            "apps/cccc3333/workspace/.lingxi/app.json",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let app = new_app("cccc3333");
            save_full(dir.path(), &[app]);
            std::fs::remove_file(dir.path().join(missing)).unwrap();
            let err = load_all(dir.path()).unwrap_err();
            assert_eq!(err.code(), AppErrorCode::StorageCorrupt, "{missing}");
        }
    }

    /// Every per-app document's `schemaVersion` guard must fire — not just the
    /// index-level one. A future v2 document loaded by a v1 binary must fail
    /// typed instead of being silently field-misinterpreted.
    #[test]
    fn unsupported_per_app_doc_schema_version_is_storage_corrupt() {
        for doc in [
            "apps/iiii9999/runtime.json",
            "apps/iiii9999/workspace/.lingxi/app.json",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let app = new_app("iiii9999");
            save_full(dir.path(), &[app]);
            let path = dir.path().join(doc);
            let body = std::fs::read_to_string(&path).unwrap();
            assert!(
                body.contains(&format!("\"schemaVersion\": {APPS_SCHEMA_VERSION}")),
                "{doc} must carry the schema version"
            );
            std::fs::write(
                &path,
                body.replacen(
                    &format!("\"schemaVersion\": {APPS_SCHEMA_VERSION}"),
                    "\"schemaVersion\": 99",
                    1,
                ),
            )
            .unwrap();
            let err = load_all(dir.path()).unwrap_err();
            assert_eq!(err.code(), AppErrorCode::StorageCorrupt, "{doc}");
            assert!(
                err.to_string().contains("unsupported schemaVersion 99"),
                "{doc}: {err}"
            );
        }
    }

    /// A document above [`MAX_DOC_BYTES`] is out of contract: it must fail
    /// typed `storage_corrupt` instead of being slurped unbounded into memory.
    #[test]
    fn oversized_document_is_storage_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let app = new_app("jjjj0000");
        save_full(dir.path(), &[app]);
        let path = dir.path().join("apps/jjjj0000/runtime.json");
        let oversized = vec![b' '; usize::try_from(MAX_DOC_BYTES).unwrap() + 1];
        std::fs::write(&path, oversized).unwrap();
        let err = load_all(dir.path()).unwrap_err();
        assert_eq!(err.code(), AppErrorCode::StorageCorrupt);
        assert!(err.to_string().contains("limit"), "{err}");
    }

    #[test]
    fn orphan_temp_files_are_ignored_and_index_survives_partial_write() {
        let dir = tempfile::tempdir().unwrap();
        let apps = vec![new_app("dddd4444")];
        save_full(dir.path(), &apps);
        // Simulate a crashed atomic write: an orphan temp with garbage next to
        // the real index (rooted_fs names temps `<final>.tmp-<pid>-<seq>`),
        // plus one inside the app directory.
        std::fs::write(
            dir.path().join("apps/index.json.tmp-1234-7"),
            "GARBAGE-PARTIAL-WRITE",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("apps/dddd4444/runtime.json.tmp-1-1"),
            "{\"half\":",
        )
        .unwrap();
        let loaded = load_all(dir.path()).unwrap();
        assert_eq!(loaded, apps);
        // A later write still lands atomically despite the orphans.
        save_index(dir.path(), &[loaded[0].record.clone()]).unwrap();
        assert_eq!(load_all(dir.path()).unwrap(), apps);
    }

    #[test]
    fn delete_app_dir_removes_only_the_contained_dir() {
        let dir = tempfile::tempdir().unwrap();
        let apps = vec![new_app("eeee5555"), new_app("ffff6666")];
        save_full(dir.path(), &apps);
        delete_app_dir(dir.path(), "eeee5555").unwrap();
        assert!(!dir.path().join("apps/eeee5555").exists());
        assert!(
            !dir.path().join("apps/eeee5555/build.lock").exists(),
            "the per-app lock is a runtime artifact and must not survive deletion"
        );
        assert!(dir.path().join("apps/ffff6666/runtime.json").is_file());
        // Deleting a missing dir is a no-op.
        delete_app_dir(dir.path(), "eeee5555").unwrap();
    }

    #[test]
    fn app_build_lock_is_confined_to_an_existing_app_directory() {
        let dir = tempfile::tempdir().unwrap();
        let app = new_app("llll2222");
        save_full(dir.path(), std::slice::from_ref(&app));

        let lock = lock_app_build(dir.path(), "llll2222").unwrap();
        assert!(dir.path().join("apps/llll2222/build.lock").is_file());
        drop(lock);

        let err = match lock_app_build(dir.path(), "mmmm3333") {
            Ok(_) => panic!("missing app directory must not be created by locking"),
            Err(error) => error,
        };
        assert_eq!(err.code(), AppErrorCode::NotFound);
        assert!(!dir.path().join("apps/mmmm3333").exists());
    }

    #[test]
    fn app_background_lock_is_confined_to_an_existing_app_directory() {
        let dir = tempfile::tempdir().unwrap();
        let app = new_app("nnnn3333");
        save_full(dir.path(), std::slice::from_ref(&app));

        let lock = lock_app_background(dir.path(), "nnnn3333").unwrap();
        assert!(dir.path().join("apps/nnnn3333/background.lock").is_file());
        drop(lock);

        let err = match lock_app_background(dir.path(), "oooo4444") {
            Ok(_) => panic!("missing app directory must not be created by locking"),
            Err(error) => error,
        };
        assert_eq!(err.code(), AppErrorCode::NotFound);
        assert!(!dir.path().join("apps/oooo4444").exists());
    }

    #[test]
    fn delete_app_dir_rejects_traversal_ids() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["../evil", "a/b", "..", "UPPER", ""] {
            let err = delete_app_dir(dir.path(), bad).unwrap_err();
            assert_eq!(err.code(), AppErrorCode::InvalidRequest, "id {bad:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn delete_app_dir_removes_a_planted_symlink_without_following_it() {
        let dir = tempfile::tempdir().unwrap();
        let victim = tempfile::tempdir().unwrap();
        std::fs::write(victim.path().join("precious.txt"), "keep me").unwrap();
        std::fs::create_dir_all(dir.path().join("apps")).unwrap();
        std::os::unix::fs::symlink(victim.path(), dir.path().join("apps/gggg7777")).unwrap();
        delete_app_dir(dir.path(), "gggg7777").unwrap();
        assert!(!dir.path().join("apps/gggg7777").exists());
        assert!(
            victim.path().join("precious.txt").is_file(),
            "symlink target must survive"
        );
    }

    /// The write seam enforces the SAME size bound loads do: a document the
    /// loader would refuse never reaches disk, so an over-cap mutation fails
    /// typed and the store on disk stays loadable.
    #[test]
    fn oversized_write_fails_typed_and_leaves_the_store_loadable() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = new_app("kkkk1111");
        save_full(dir.path(), std::slice::from_ref(&app));

        let over = usize::try_from(MAX_DOC_BYTES).unwrap() + 1;
        app.runtime.last_error = Some("x".repeat(over));
        let err = save_runtime(dir.path(), "kkkk1111", &app.runtime).unwrap_err();
        assert_eq!(err.code(), AppErrorCode::InvalidRequest);
        assert!(
            err.to_string().contains("durable size limit"),
            "unexpected message: {err}"
        );

        // Disk was never touched: the store still loads cleanly and holds the
        // committed (error-free) runtime record.
        let loaded = load_all(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].runtime.last_error.is_none());
    }

    #[test]
    fn write_order_is_the_canonical_two_step_sequence() {
        assert_eq!(
            APP_DOC_WRITE_ORDER,
            [AppDocWriteStep::Runtime, AppDocWriteStep::MetadataMirror],
            "the canonical order is part of the repair contract"
        );
        assert_eq!(
            write_order_prefix_through(AppDocWriteStep::Runtime),
            &APP_DOC_WRITE_ORDER[..1]
        );
        assert_eq!(
            write_order_prefix_through(AppDocWriteStep::MetadataMirror),
            &APP_DOC_WRITE_ORDER[..]
        );
    }

    /// Behavioral pin of the write ORDER (not just the const): failing one
    /// document's write mid-batch must leave exactly the canonical prefix
    /// updated. Reordering `save_app_files` breaks this test.
    #[cfg(unix)]
    #[test]
    fn save_app_files_executes_the_canonical_write_order() {
        let dir = tempfile::tempdir().unwrap();
        let before = new_app("mmmm3333");
        save_full(dir.path(), std::slice::from_ref(&before));

        // A distinct after-state in both documents.
        let mut after = before.clone();
        after
            .set_runtime(
                AppRuntimeState::Starting,
                Some(3999),
                Some(7),
                None,
                1_700_000_000_101,
            )
            .unwrap();
        after.record.name = "After".into();
        after.record.updated_at_ms = 1_700_000_000_101;

        // Fail at runtime.json (the FIRST step): the mirror (later) must not
        // be written.
        let runtime_path = dir.path().join(runtime_rel("mmmm3333"));
        std::fs::remove_file(&runtime_path).unwrap();
        std::os::unix::fs::symlink("/dev/null", &runtime_path).unwrap();
        save_app_files(dir.path(), &after).unwrap_err();
        let mirror_body =
            std::fs::read_to_string(dir.path().join(metadata_rel("mmmm3333"))).unwrap();
        assert!(
            !mirror_body.contains("\"After\""),
            "the mirror must not be written before runtime"
        );
    }

    /// Finding 8: a runtime record stranded busy by a crash is reconciled at
    /// load (no runtime process outlives the engine) and the reconciliation
    /// is persisted.
    #[test]
    fn stranded_busy_runtime_states_are_reconciled_at_load() {
        use crate::types::AppRuntimeState::{Failed, Running, Starting, Stopped, Stopping};
        let cases = [
            (Starting, Failed, true),
            (Running, Failed, true),
            (Stopping, Stopped, false),
        ];
        for (stranded, expected, expect_error) in cases {
            let dir = tempfile::tempdir().unwrap();
            let mut app = new_app("nnnn4444");
            // Walk legal runtime transitions up to the stranded state.
            app.set_runtime(Starting, Some(3123), Some(42), None, 2)
                .unwrap();
            if matches!(stranded, Running | Stopping) {
                app.set_runtime(Running, None, Some(42), None, 3).unwrap();
            }
            if stranded == Stopping {
                app.set_runtime(Stopping, None, Some(42), None, 4).unwrap();
            }
            save_full(dir.path(), std::slice::from_ref(&app));

            let loaded = load_all(dir.path()).unwrap();
            assert_eq!(loaded[0].runtime.state, expected, "{stranded}");
            assert_eq!(
                loaded[0].runtime.last_error.as_deref(),
                expect_error.then_some("reconciled at load: no live runtime manager"),
                "{stranded}"
            );
            assert_eq!(loaded[0].runtime.port, Some(3123), "port pin survives");
            // The reconciliation is persisted: a second load sees the exact
            // same (already consistent) state.
            assert_eq!(load_all(dir.path()).unwrap(), loaded, "{stranded}");
            let body =
                std::fs::read_to_string(dir.path().join("apps/nnnn4444/runtime.json")).unwrap();
            assert!(
                body.contains(&format!("\"{expected}\"")),
                "{stranded}: {body}"
            );
        }
        // stopped / failed records are untouched (no rewrite).
        let dir = tempfile::tempdir().unwrap();
        let app = new_app("nnnn4444");
        save_full(dir.path(), std::slice::from_ref(&app));
        let before =
            std::fs::read_to_string(dir.path().join("apps/nnnn4444/runtime.json")).unwrap();
        assert_eq!(load_all(dir.path()).unwrap(), vec![app]);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("apps/nnnn4444/runtime.json")).unwrap(),
            before,
            "a settled runtime record must load read-only"
        );
    }

    /// Finding 11: a non-UTF-8 document is store tampering/corruption, not a
    /// generic I/O failure.
    #[test]
    fn non_utf8_document_is_storage_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let app = new_app("oooo5555");
        save_full(dir.path(), std::slice::from_ref(&app));
        std::fs::write(
            dir.path().join("apps/oooo5555/runtime.json"),
            [0xff, 0xfe, 0x00, 0x01],
        )
        .unwrap();
        let err = load_all(dir.path()).unwrap_err();
        assert_eq!(err.code(), AppErrorCode::StorageCorrupt, "{err}");
        assert!(err.to_string().contains("not valid UTF-8"), "{err}");

        // Same contract for the index document itself.
        std::fs::write(dir.path().join("apps/index.json"), [0xff, 0xfe]).unwrap();
        let err = load_all(dir.path()).unwrap_err();
        assert_eq!(err.code(), AppErrorCode::StorageCorrupt, "{err}");
    }

    /// Finding 11: a symlink squatting on a document path is store tampering
    /// (`storage_corrupt`), not a malformed caller request.
    #[cfg(unix)]
    #[test]
    fn symlink_squatting_on_a_document_path_is_storage_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let victim = tempfile::tempdir().unwrap();
        std::fs::write(victim.path().join("payload.json"), "{}").unwrap();
        let app = new_app("pppp6666");
        save_full(dir.path(), std::slice::from_ref(&app));
        let target = dir.path().join("apps/pppp6666/runtime.json");
        std::fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink(victim.path().join("payload.json"), &target).unwrap();
        let err = load_all(dir.path()).unwrap_err();
        assert_eq!(err.code(), AppErrorCode::StorageCorrupt, "{err}");
        assert!(err.to_string().contains("regular file"), "{err}");
    }
}
