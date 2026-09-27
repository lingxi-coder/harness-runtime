//! Git-backed checkpoints for one local application's source workspace.
//!
//! The repository lives inside `apps/<id>/workspace`, while the `SQLite`
//! database and runtime artifacts live beside it. Restoring a checkpoint can
//! therefore replace source files without ever touching application data.
//!
//! The service's OWN documents do live inside that directory
//! (`workspace/.lingxi/{app.json,app.manifest.json}`): they
//! are application STATE, not generated source, and a restore must rewind the
//! source without rewinding the store that records which checkpoint the app
//! is on. Keeping them out takes three cooperating rules, because each covers
//! a case the others cannot:
//!
//! * [`exclude_service_documents`] stops them being staged at all — but only
//!   while they are untracked;
//! * [`untrack_service_documents`] drops them from the index of a repository
//!   that already tracks them, which every workspace created before those
//!   rules existed does;
//! * [`AppCheckpointStore::restore`] preserves them across the hard reset,
//!   because a checkpoint COMMITTED by that older build still carries the
//!   blobs in its tree and a reset onto it re-materialises them.

use crate::error::AppError;
use crate::manifest::AppLayout;
use crate::types::{AppCheckpoint, AppCheckpointKind};
use git2::{build::CheckoutBuilder, IndexAddOption, Oid, Repository, ResetType, Signature, Time};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const CHECKPOINT_REF_PREFIX: &str = "refs/lingxi/checkpoints";
const CHECKPOINT_TRAILER: &str = "Lingxi-Checkpoint:";
const CREATED_AT_TRAILER: &str = "Lingxi-Created-At-Ms:";
const DEPENDENCY_LOCK_TRAILER: &str = "Lingxi-Dependency-Lock-Digest:";
const DEPENDENCY_LOCK_FILE: &str = "pnpm-lock.yaml";

/// Git checkpoint operations scoped to a validated app workspace.
#[derive(Debug, Clone)]
pub struct AppCheckpointStore {
    workspace: PathBuf,
}

impl AppCheckpointStore {
    /// Resolve a checkpoint store from the app's validated layout.
    #[must_use]
    pub fn new(layout: &AppLayout) -> Self {
        Self {
            workspace: layout.root().join(layout.workspace_rel()),
        }
    }

    /// Initialize the workspace repository if necessary and commit its exact
    /// current contents. A private ref keeps every checkpoint reachable even
    /// after a later restore moves `HEAD` backwards.
    pub fn create(
        &self,
        kind: AppCheckpointKind,
        label: &str,
        created_at_ms: u64,
    ) -> Result<AppCheckpoint, AppError> {
        validate_label(label)?;
        reject_workspace_symlinks(&self.workspace)?;
        let repo = self.open_or_init()?;
        let mut index = repo.index().map_err(git_error("open checkpoint index"))?;
        index
            .add_all(["*"], IndexAddOption::DEFAULT, None)
            .map_err(git_error("stage checkpoint workspace"))?;
        index.write().map_err(git_error("write checkpoint index"))?;
        let tree_oid = index
            .write_tree()
            .map_err(git_error("write checkpoint tree"))?;
        let tree = repo
            .find_tree(tree_oid)
            .map_err(git_error("read checkpoint tree"))?;
        let parent = repo.head().ok().and_then(|head| head.peel_to_commit().ok());
        let signature = signature(created_at_ms)?;
        let dependency_lock_digest =
            dependency_lock_digest(&self.workspace.join(DEPENDENCY_LOCK_FILE))?;
        let dependency_lock_trailer = dependency_lock_digest
            .as_deref()
            .map(|digest| format!("{DEPENDENCY_LOCK_TRAILER} {digest}\n"))
            .unwrap_or_default();
        let message = format!(
            "{label}\n\n{CHECKPOINT_TRAILER} {}\n{CREATED_AT_TRAILER} {created_at_ms}\n{dependency_lock_trailer}",
            kind_name(kind)
        );
        let oid = match parent.as_ref() {
            Some(parent) => repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                &message,
                &tree,
                &[parent],
            ),
            None => repo.commit(Some("HEAD"), &signature, &signature, &message, &tree, &[]),
        }
        .map_err(git_error("commit checkpoint"))?;

        let reference = format!(
            "{CHECKPOINT_REF_PREFIX}/{created_at_ms}-{}-{:.12}",
            kind_name(kind),
            oid
        );
        repo.reference(&reference, oid, true, "record Lingxi app checkpoint")
            .map_err(git_error("retain checkpoint reference"))?;
        Ok(AppCheckpoint {
            id: oid.to_string(),
            label: label.to_string(),
            kind,
            created_at_ms,
        })
    }

    /// Read the pnpm dependency-lock digest captured by a retained checkpoint.
    /// Older checkpoints return `None`, which is treated as an explicit
    /// unknown rather than silently equivalent to the current generation.
    ///
    /// The trailer records the digest of the lockfile that was on disk when the
    /// checkpoint was taken. Missing or malformed trailers are deliberately
    /// treated as unknown; this format does not retain legacy lockfile fallback
    /// behavior.
    pub fn dependency_lock_digest(&self, checkpoint_id: &str) -> Result<Option<String>, AppError> {
        let target = parse_oid(checkpoint_id)?;
        let repo =
            Repository::open(&self.workspace).map_err(git_error("open checkpoint repository"))?;
        let commit = repo
            .find_commit(target)
            .map_err(git_error("read checkpoint commit"))?;
        if let Some(digest) = commit
            .message()
            .ok()
            .and_then(|message| trailer(message, DEPENDENCY_LOCK_TRAILER))
            .filter(|digest| is_sha256_hex(digest))
        {
            return Ok(Some(digest.to_string()));
        }
        Ok(None)
    }

    /// Read the pnpm dependency-lock digest currently present in the app workspace.
    pub fn current_dependency_lock_digest(&self) -> Result<Option<String>, AppError> {
        dependency_lock_digest(&self.workspace.join(DEPENDENCY_LOCK_FILE))
    }

    /// List every retained checkpoint, newest first.
    pub fn list(&self) -> Result<Vec<AppCheckpoint>, AppError> {
        let repo = match Repository::open(&self.workspace) {
            Ok(repo) => repo,
            Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(AppError::Io(format!("open checkpoint repository: {error}"))),
        };
        let mut checkpoints = Vec::new();
        let refs = repo
            .references_glob(&format!("{CHECKPOINT_REF_PREFIX}/*"))
            .map_err(git_error("list checkpoint references"))?;
        for reference in refs {
            let reference = reference.map_err(git_error("read checkpoint reference"))?;
            let commit = reference
                .peel_to_commit()
                .map_err(git_error("read checkpoint commit"))?;
            checkpoints.push(parse_checkpoint(&commit)?);
        }
        checkpoints.sort_by(|a, b| {
            b.created_at_ms
                .cmp(&a.created_at_ms)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(checkpoints)
    }

    /// Create a `pre_restore` safety checkpoint, then restore the requested
    /// checkpoint with a forced checkout that also removes untracked source
    /// files. Paths outside the workspace (including `data/app.sqlite`) are
    /// outside the Git repository and cannot be modified by this operation.
    ///
    /// The service's own documents under `workspace/.lingxi/` ARE inside it.
    /// For a checkpoint written by today's build they are neither tracked nor
    /// removable by this checkout (which asks for `remove_untracked`, not
    /// `remove_ignored`), but a checkpoint written before those rules existed
    /// still carries them in its tree, and the reset would check those blobs
    /// back out over the live ones. So the directory is snapshotted before the
    /// reset and put back afterwards: whatever the target commit says, the
    /// service state on disk is the same before and after.
    pub fn restore(
        &self,
        checkpoint_id: &str,
        created_at_ms: u64,
    ) -> Result<AppCheckpoint, AppError> {
        let target = parse_oid(checkpoint_id)?;
        let known = self.list()?;
        if !known
            .iter()
            .any(|checkpoint| checkpoint.id == checkpoint_id)
        {
            return Err(AppError::NotFound(format!(
                "checkpoint {checkpoint_id} was not found"
            )));
        }
        let safety = self.create(
            AppCheckpointKind::PreRestore,
            "Safety checkpoint before restore",
            created_at_ms,
        )?;
        let repo =
            Repository::open(&self.workspace).map_err(git_error("open checkpoint repository"))?;
        let object = repo
            .find_object(target, None)
            .map_err(git_error("read restore target"))?;
        let mut checkout = CheckoutBuilder::new();
        checkout.force().remove_untracked(true);
        let preserved = read_service_documents(&self.workspace)?;
        repo.reset(&object, ResetType::Hard, Some(&mut checkout))
            .map_err(git_error("restore checkpoint"))?;
        restore_service_documents(&self.workspace, &preserved)?;
        Ok(safety)
    }

    fn open_or_init(&self) -> Result<Repository, AppError> {
        std::fs::create_dir_all(&self.workspace).map_err(|error| {
            AppError::Io(format!(
                "create checkpoint workspace {}: {error}",
                self.workspace.display()
            ))
        })?;
        let repo = match Repository::open(&self.workspace) {
            Ok(repo) => repo,
            Err(error) if error.code() == git2::ErrorCode::NotFound => {
                Repository::init(&self.workspace)
                    .map_err(git_error("initialize checkpoint repository"))?
            }
            Err(error) => return Err(AppError::Io(format!("open checkpoint repository: {error}"))),
        };
        exclude_service_documents(&repo)?;
        untrack_service_documents(&repo)?;
        Ok(repo)
    }
}

/// Keep `workspace/.lingxi/` — the service's record mirror, design draft and
/// data manifest — out of the checkpointed tree.
///
/// `create` stages everything with `add_all(["*"])`, and `restore` resets
/// hard, so without this the git history would carry the store's own state
/// and a restore would silently rewind it: the user's design draft back to
/// its checkpoint-era revision (never rewritten afterwards, because the
/// service persists only the documents whose IN-MEMORY value changed) and
/// the data manifest out of agreement with the database it is hashed into.
///
/// This rule alone is enough only for a repository this code created, where
/// `.lingxi/` has never been tracked: `add_all` with `IndexAddOption::DEFAULT`
/// skips ignored paths, and the restore checkout asks for `remove_untracked`,
/// not `remove_ignored`. It is written on every open rather than only at
/// `Repository::init` so that an older repository gains the rule too — but
/// gaining it is not the same as being migrated by it, because Git consults
/// ignore rules only for UNTRACKED paths. [`untrack_service_documents`] is
/// what actually migrates such a repository.
fn exclude_service_documents(repo: &Repository) -> Result<(), AppError> {
    let info = repo.path().join("info");
    std::fs::create_dir_all(&info).map_err(|error| {
        AppError::Io(format!(
            "create checkpoint exclude directory {}: {error}",
            info.display()
        ))
    })?;
    let exclude = info.join("exclude");
    let pattern = format!("/{}/\n", crate::storage::APP_STATE_DIR);
    if std::fs::read_to_string(&exclude).is_ok_and(|body| body == pattern) {
        return Ok(());
    }
    std::fs::write(&exclude, pattern).map_err(|error| {
        AppError::Io(format!(
            "write checkpoint exclude {}: {error}",
            exclude.display()
        ))
    })
}

/// Drop `workspace/.lingxi/` from the INDEX of a repository that still tracks
/// it.
///
/// Every workspace created before [`exclude_service_documents`] existed
/// committed the service documents through `add_all(["*"])`. Git applies
/// ignore rules only to untracked paths, so for those repositories the
/// exclude is inert: `add_all` keeps refreshing the tracked entries into each
/// new checkpoint and the restore keeps rewinding them. Removing the entries
/// migrates the repository for real.
///
/// Idempotent and cheap enough for every open: a repository that does not
/// track them writes nothing.
fn untrack_service_documents(repo: &Repository) -> Result<(), AppError> {
    let mut index = repo.index().map_err(git_error("open checkpoint index"))?;
    let prefix = format!("{}/", crate::storage::APP_STATE_DIR).into_bytes();
    let tracked = index.iter().any(|entry| entry.path.starts_with(&prefix));
    if !tracked {
        return Ok(());
    }
    index
        .remove_dir(Path::new(crate::storage::APP_STATE_DIR), 0)
        .map_err(git_error("untrack checkpoint service documents"))?;
    index.write().map_err(git_error("write checkpoint index"))?;
    Ok(())
}

/// One file under `workspace/.lingxi/`, carried across a hard reset.
struct ServiceDocument {
    /// Path relative to `workspace/.lingxi/`.
    relative: PathBuf,
    /// Exact bytes as they were before the reset.
    bytes: Vec<u8>,
    /// Mode as it was before the reset (the directory is private, and a
    /// rewrite must not widen its files).
    permissions: std::fs::Permissions,
}

/// Read every file under `workspace/.lingxi/`. A missing directory is not an
/// error — an app whose store has written nothing yet simply has no documents
/// to preserve.
fn read_service_documents(workspace: &Path) -> Result<Vec<ServiceDocument>, AppError> {
    let state_dir = workspace.join(crate::storage::APP_STATE_DIR);
    let mut documents = Vec::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(relative) = pending.pop() {
        let directory = state_dir.join(&relative);
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(AppError::Io(format!(
                    "read service documents {}: {error}",
                    directory.display()
                )))
            }
        };
        for entry in entries {
            let entry = entry
                .map_err(|error| AppError::Io(format!("read service document entry: {error}")))?;
            let path = entry.path();
            // `DirEntry::metadata` does not follow symlinks, and `create`
            // rejects workspace symlinks outright, so neither branch is taken
            // for one.
            let metadata = entry
                .metadata()
                .map_err(|error| AppError::Io(format!("inspect {}: {error}", path.display())))?;
            let child = relative.join(entry.file_name());
            if metadata.is_dir() {
                pending.push(child);
            } else if metadata.is_file() {
                let bytes = std::fs::read(&path)
                    .map_err(|error| AppError::Io(format!("read {}: {error}", path.display())))?;
                documents.push(ServiceDocument {
                    relative: child,
                    bytes,
                    permissions: metadata.permissions(),
                });
            }
        }
    }
    Ok(documents)
}

/// Put `preserved` back exactly, discarding anything else the reset left
/// under `workspace/.lingxi/`.
///
/// Files whose bytes already match are left untouched, so restoring an app
/// whose checkpoints all come from today's build rewrites nothing.
fn restore_service_documents(
    workspace: &Path,
    preserved: &[ServiceDocument],
) -> Result<(), AppError> {
    let state_dir = workspace.join(crate::storage::APP_STATE_DIR);
    for present in read_service_documents(workspace)? {
        if preserved
            .iter()
            .any(|document| document.relative == present.relative)
        {
            continue;
        }
        let path = state_dir.join(&present.relative);
        std::fs::remove_file(&path).map_err(|error| {
            AppError::Io(format!(
                "discard restored service document {}: {error}",
                path.display()
            ))
        })?;
    }
    for document in preserved {
        let path = state_dir.join(&document.relative);
        if std::fs::read(&path).is_ok_and(|current| current == document.bytes) {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                AppError::Io(format!(
                    "recreate service document directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        std::fs::write(&path, &document.bytes).map_err(|error| {
            AppError::Io(format!(
                "preserve service document {}: {error}",
                path.display()
            ))
        })?;
        std::fs::set_permissions(&path, document.permissions.clone()).map_err(|error| {
            AppError::Io(format!(
                "preserve service document mode {}: {error}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

fn parse_checkpoint(commit: &git2::Commit<'_>) -> Result<AppCheckpoint, AppError> {
    let message = commit.message().map_err(|error| {
        AppError::StorageCorrupt(format!(
            "checkpoint commit {} has no UTF-8 message: {error}",
            commit.id()
        ))
    })?;
    let label = message.lines().next().unwrap_or_default().trim();
    validate_label(label).map_err(|error| {
        AppError::StorageCorrupt(format!("invalid checkpoint {}: {error}", commit.id()))
    })?;
    let kind = trailer(message, CHECKPOINT_TRAILER)
        .and_then(parse_kind)
        .ok_or_else(|| {
            AppError::StorageCorrupt(format!(
                "checkpoint commit {} has no valid kind trailer",
                commit.id()
            ))
        })?;
    let created_at_ms = trailer(message, CREATED_AT_TRAILER)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            AppError::StorageCorrupt(format!(
                "checkpoint commit {} has no valid creation timestamp",
                commit.id()
            ))
        })?;
    Ok(AppCheckpoint {
        id: commit.id().to_string(),
        label: label.to_string(),
        kind,
        created_at_ms,
    })
}

fn trailer<'a>(message: &'a str, prefix: &str) -> Option<&'a str> {
    message
        .lines()
        .find_map(|line| line.strip_prefix(prefix).map(str::trim))
}

fn kind_name(kind: AppCheckpointKind) -> &'static str {
    match kind {
        AppCheckpointKind::ScaffoldCreated => "scaffold_created",
        AppCheckpointKind::GenerationValidated => "generation_validated",
        AppCheckpointKind::PreviewApproved => "preview_approved",
        AppCheckpointKind::UserApproved => "user_approved",
        AppCheckpointKind::PreRestore => "pre_restore",
    }
}

fn parse_kind(value: &str) -> Option<AppCheckpointKind> {
    match value {
        "scaffold_created" => Some(AppCheckpointKind::ScaffoldCreated),
        "generation_validated" => Some(AppCheckpointKind::GenerationValidated),
        "preview_approved" => Some(AppCheckpointKind::PreviewApproved),
        "user_approved" => Some(AppCheckpointKind::UserApproved),
        "pre_restore" => Some(AppCheckpointKind::PreRestore),
        _ => None,
    }
}

fn signature(created_at_ms: u64) -> Result<Signature<'static>, AppError> {
    let seconds = i64::try_from(created_at_ms / 1_000)
        .map_err(|_| AppError::InvalidRequest("checkpoint timestamp is out of range".into()))?;
    Signature::new(
        "Lingxi Local Apps",
        "local-apps@lingxi.invalid",
        &Time::new(seconds, 0),
    )
    .map_err(git_error("create checkpoint signature"))
}

fn validate_label(label: &str) -> Result<(), AppError> {
    if label.trim().is_empty() || label.len() > 500 || label.contains(['\n', '\r']) {
        Err(AppError::InvalidRequest(
            "checkpoint label must be one non-empty line of at most 500 bytes".into(),
        ))
    } else {
        Ok(())
    }
}

fn parse_oid(value: &str) -> Result<Oid, AppError> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::InvalidRequest(
            "checkpoint id must be a full 40-character Git object id".into(),
        ));
    }
    Oid::from_str(value)
        .map_err(|error| AppError::InvalidRequest(format!("invalid checkpoint id: {error}")))
}

/// A commit message is free text a user can edit with `git commit --amend`, so
/// the trailer is only trusted when it is shaped like the digest this module
/// writes. Anything else falls through to the commit tree.
fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn dependency_lock_digest(path: &Path) -> Result<Option<String>, AppError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(format!("{:x}", Sha256::digest(bytes)))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(AppError::Io(format!("read pnpm-lock.yaml: {error}"))),
    }
}

fn reject_workspace_symlinks(workspace: &Path) -> Result<(), AppError> {
    if !workspace.exists() {
        return Ok(());
    }
    let mut pending = vec![workspace.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = std::fs::read_dir(&directory).map_err(|error| {
            AppError::Io(format!(
                "inspect checkpoint workspace {}: {error}",
                directory.display()
            ))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                AppError::Io(format!("inspect checkpoint workspace entry: {error}"))
            })?;
            let kind = entry.file_type().map_err(|error| {
                AppError::Io(format!("inspect {}: {error}", entry.path().display()))
            })?;
            if entry.file_name() == ".git" {
                if kind.is_symlink() || !kind.is_dir() {
                    return Err(AppError::InvalidRequest(
                        "workspace .git must be a real directory".into(),
                    ));
                }
                continue;
            }
            if kind.is_symlink() {
                return Err(AppError::InvalidRequest(format!(
                    "workspace symlink is not allowed: {}",
                    entry.path().display()
                )));
            }
            if kind.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}

fn git_error(operation: &'static str) -> impl FnOnce(git2::Error) -> AppError {
    move |error| AppError::Io(format!("{operation}: {error}"))
}

/// Seed the repository the PRE-FIX build left behind: a checkpoint whose tree
/// carries `workspace/.lingxi/*`, plus the retained ref that makes it
/// restorable. This is the whole point of the legacy tests — a store that
/// creates its own repository can never reach this state, so a fixture built
/// by [`AppCheckpointStore::create`] alone is structurally blind to the
/// defect. Reproduced exactly as `create` did it before the exclusion existed:
/// `add_all(["*"])` with no `.git/info/exclude` and no index removal.
///
/// Returns the checkpoint id, which [`AppCheckpointStore::restore`] accepts.
#[cfg(test)]
pub(crate) fn seed_legacy_checkpoint(workspace: &Path, created_at_ms: u64) -> String {
    let repo = Repository::init(workspace).unwrap();
    let mut index = repo.index().unwrap();
    index.add_all(["*"], IndexAddOption::DEFAULT, None).unwrap();
    index.write().unwrap();
    let tree_oid = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_oid).unwrap();
    let signature = signature(created_at_ms).unwrap();
    let message = format!(
        "legacy checkpoint\n\n{CHECKPOINT_TRAILER} scaffold_created\n\
         {CREATED_AT_TRAILER} {created_at_ms}\n"
    );
    let oid = repo
        .commit(Some("HEAD"), &signature, &signature, &message, &tree, &[])
        .unwrap();
    repo.reference(
        &format!("{CHECKPOINT_REF_PREFIX}/{created_at_ms}-scaffold_created-legacy"),
        oid,
        true,
        "seed legacy checkpoint",
    )
    .unwrap();
    // Guard the fixture itself: if this ever stops staging the service
    // documents, every legacy test would pass for the wrong reason.
    assert!(
        tree.get_name(crate::storage::APP_STATE_DIR).is_some(),
        "the legacy fixture must commit the service documents"
    );
    oid.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn layout(root: &Path) -> AppLayout {
        let layout = AppLayout::new(root, "app-test").unwrap();
        layout.initialize().unwrap();
        layout
    }

    fn tracked_paths(workspace: &Path) -> Vec<String> {
        let repo = Repository::open(workspace).unwrap();
        let index = repo.index().unwrap();
        index
            .iter()
            .map(|entry| String::from_utf8(entry.path).unwrap())
            .collect()
    }

    fn committed_paths(workspace: &Path, checkpoint_id: &str) -> Vec<String> {
        let repo = Repository::open(workspace).unwrap();
        let tree = repo
            .find_commit(parse_oid(checkpoint_id).unwrap())
            .unwrap()
            .tree()
            .unwrap();
        let mut committed = Vec::new();
        tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() == Some(git2::ObjectType::Blob) {
                committed.push(format!("{dir}{}", entry.name().unwrap_or_default()));
            }
            git2::TreeWalkResult::Ok
        })
        .unwrap();
        committed
    }

    /// A restore must rewind generated SOURCE and nothing else. The database
    /// lives outside the repository (`apps/<id>/data/`), so no git operation
    /// can reach it — but the service's own documents live INSIDE the
    /// workspace at `.lingxi/`, where the hard reset could reach them; they
    /// are the part of this contract a code change can actually break.
    #[test]
    fn restore_replaces_workspace_but_never_database_or_service_documents() {
        let root = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        let workspace = root.path().join(layout.workspace_rel());
        let state_dir = workspace.join(crate::storage::APP_STATE_DIR);
        let mirror = state_dir.join(crate::storage::APP_METADATA_FILE);
        let draft = state_dir.join("design-spec.json");
        let manifest = root.path().join(layout.manifest_rel());
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(workspace.join("page.js"), "one").unwrap();
        fs::write(&mirror, "record-v1").unwrap();
        fs::write(&draft, "draft-v1").unwrap();
        fs::write(&manifest, "manifest-v1").unwrap();
        let database = layout.database_path();
        fs::write(&database, "data-v1").unwrap();
        let store = AppCheckpointStore::new(&layout);
        let first = store
            .create(AppCheckpointKind::ScaffoldCreated, "first", 1_000)
            .unwrap();
        fs::write(workspace.join("page.js"), "two").unwrap();
        fs::write(workspace.join("extra.js"), "remove me").unwrap();
        fs::write(&mirror, "record-v2").unwrap();
        fs::write(&draft, "draft-v2").unwrap();
        fs::write(&manifest, "manifest-v2").unwrap();
        fs::write(&database, "data-v2").unwrap();
        store
            .create(AppCheckpointKind::GenerationValidated, "second", 2_000)
            .unwrap();

        let safety = store.restore(&first.id, 3_000).unwrap();
        assert_eq!(safety.kind, AppCheckpointKind::PreRestore);
        assert_eq!(
            fs::read_to_string(workspace.join("page.js")).unwrap(),
            "one"
        );
        assert!(!workspace.join("extra.js").exists());
        // The store's own state is not source and is never rewound.
        assert_eq!(fs::read_to_string(&mirror).unwrap(), "record-v2");
        assert_eq!(fs::read_to_string(&draft).unwrap(), "draft-v2");
        assert_eq!(fs::read_to_string(&manifest).unwrap(), "manifest-v2");
        assert_eq!(fs::read_to_string(database).unwrap(), "data-v2");
        assert_eq!(store.list().unwrap().len(), 3);
    }

    #[test]
    fn checkpoint_captures_and_reads_workspace_dependency_lock_digest() {
        let root = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        let workspace = root.path().join(layout.workspace_rel());
        fs::create_dir_all(&workspace).unwrap();
        fs::write(
            workspace.join("package.json"),
            br#"{"name":"demo","dependencies":{}}"#,
        )
        .unwrap();
        let lockfile = workspace.join(DEPENDENCY_LOCK_FILE);
        fs::write(&lockfile, br#"{"lockfileVersion":3,"packages":{}}"#).unwrap();
        let store = AppCheckpointStore::new(&layout);
        let checkpoint = store
            .create(
                AppCheckpointKind::GenerationValidated,
                "with package lock",
                4_000,
            )
            .unwrap();
        let expected = dependency_lock_digest(&lockfile).unwrap().unwrap();
        assert_eq!(
            store.dependency_lock_digest(&checkpoint.id).unwrap(),
            Some(expected)
        );
        assert_eq!(
            store.current_dependency_lock_digest().unwrap(),
            store.dependency_lock_digest(&checkpoint.id).unwrap()
        );
        fs::write(
            &lockfile,
            br#"{"lockfileVersion":3,"packages":{"demo":{}}}"#,
        )
        .unwrap();
        assert_ne!(
            store.current_dependency_lock_digest().unwrap(),
            store.dependency_lock_digest(&checkpoint.id).unwrap()
        );
        assert!(committed_paths(&workspace, &checkpoint.id).contains(&"package.json".into()));
        assert!(committed_paths(&workspace, &checkpoint.id).contains(&"pnpm-lock.yaml".into()));
    }

    /// The exclusion has to hold at the INDEX too, not just at checkout: a
    /// staged `.lingxi/` would put the store's state into the checkpoint
    /// history, where a later restore of an older checkpoint would revert it.
    #[test]
    fn service_documents_are_never_staged_into_a_checkpoint() {
        let root = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        let workspace = root.path().join(layout.workspace_rel());
        let state_dir = workspace.join(crate::storage::APP_STATE_DIR);
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(workspace.join("page.js"), "one").unwrap();
        fs::write(state_dir.join(crate::storage::APP_METADATA_FILE), "{}").unwrap();
        fs::write(state_dir.join("design-spec.json"), "{}").unwrap();
        let checkpoint = AppCheckpointStore::new(&layout)
            .create(AppCheckpointKind::ScaffoldCreated, "first", 1_000)
            .unwrap();

        let repo = Repository::open(&workspace).unwrap();
        let tree = repo
            .find_commit(parse_oid(&checkpoint.id).unwrap())
            .unwrap()
            .tree()
            .unwrap();
        let mut committed = Vec::new();
        tree.walk(git2::TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() == Some(git2::ObjectType::Blob) {
                committed.push(format!("{dir}{}", entry.name().unwrap_or_default()));
            }
            git2::TreeWalkResult::Ok
        })
        .unwrap();
        assert_eq!(committed, vec!["page.js".to_string()]);
    }

    /// `.git/info/exclude` governs UNTRACKED paths only. A workspace created
    /// by the pre-exclusion build already has `.lingxi/` in its index, so the
    /// exclude cannot reach it and every later `add_all(["*"])` keeps
    /// refreshing it into the checkpoint history. Opening such a repository
    /// has to MIGRATE it — drop the entries from the index — not merely write
    /// a rule that no longer applies.
    #[test]
    fn a_legacy_repository_stops_tracking_service_documents_on_the_next_checkpoint() {
        let root = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        let workspace = root.path().join(layout.workspace_rel());
        let state_dir = workspace.join(crate::storage::APP_STATE_DIR);
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(workspace.join("page.js"), "one").unwrap();
        fs::write(
            state_dir.join(crate::storage::APP_METADATA_FILE),
            "record-v1",
        )
        .unwrap();
        fs::write(state_dir.join("design-spec.json"), "draft-v1").unwrap();
        seed_legacy_checkpoint(&workspace, 1_000);
        assert!(
            tracked_paths(&workspace)
                .iter()
                .any(|path| path.starts_with(".lingxi/")),
            "fixture precondition: the legacy repository tracks the service documents"
        );

        let next = AppCheckpointStore::new(&layout)
            .create(AppCheckpointKind::GenerationValidated, "second", 2_000)
            .unwrap();

        assert_eq!(
            committed_paths(&workspace, &next.id),
            vec!["page.js".to_string()]
        );
        assert_eq!(tracked_paths(&workspace), vec!["page.js".to_string()]);
    }

    /// The half untracking alone cannot cover: the LEGACY checkpoint's tree
    /// still carries the service documents, so a hard reset onto it
    /// re-materialises those blobs over the live ones. Restoring the source of
    /// a pre-exclusion app must still leave the user's design draft, record
    /// mirror and data manifest exactly as they were.
    #[test]
    fn restoring_a_legacy_checkpoint_does_not_rewind_service_documents() {
        let root = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        let workspace = root.path().join(layout.workspace_rel());
        let state_dir = workspace.join(crate::storage::APP_STATE_DIR);
        let mirror = state_dir.join(crate::storage::APP_METADATA_FILE);
        let draft = state_dir.join("design-spec.json");
        let manifest = root.path().join(layout.manifest_rel());
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(workspace.join("page.js"), "one").unwrap();
        fs::write(&mirror, "record-v1").unwrap();
        fs::write(&draft, "draft-v1").unwrap();
        fs::write(&manifest, "manifest-v1").unwrap();
        let legacy = seed_legacy_checkpoint(&workspace, 1_000);

        // Today's build takes over the same repository and the app moves on.
        let store = AppCheckpointStore::new(&layout);
        fs::write(workspace.join("page.js"), "two").unwrap();
        fs::write(&mirror, "record-v2").unwrap();
        fs::write(&draft, "draft-v2").unwrap();
        fs::write(&manifest, "manifest-v2").unwrap();
        store
            .create(AppCheckpointKind::GenerationValidated, "second", 2_000)
            .unwrap();

        store.restore(&legacy, 3_000).unwrap();

        assert_eq!(
            fs::read_to_string(workspace.join("page.js")).unwrap(),
            "one",
            "source still rewinds"
        );
        assert_eq!(fs::read_to_string(&mirror).unwrap(), "record-v2");
        assert_eq!(fs::read_to_string(&draft).unwrap(), "draft-v2");
        assert_eq!(fs::read_to_string(&manifest).unwrap(), "manifest-v2");

        // The reset re-read the index from the legacy tree, so the repository
        // is momentarily tracking them again. The next open has to heal that,
        // or the next checkpoint re-commits the documents and the whole cycle
        // starts over one restore later.
        let next = store
            .create(AppCheckpointKind::UserApproved, "third", 4_000)
            .unwrap();
        assert_eq!(
            committed_paths(&workspace, &next.id),
            vec!["page.js".to_string()]
        );
    }

    /// A `.lingxi/` entry that exists only in the legacy tree must not survive
    /// the restore either: the reset materialises it, and it was never part of
    /// the live service state that the restore is obliged to preserve.
    #[test]
    fn restoring_a_legacy_checkpoint_discards_service_documents_it_materialises() {
        let root = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        let workspace = root.path().join(layout.workspace_rel());
        let state_dir = workspace.join(crate::storage::APP_STATE_DIR);
        let draft = state_dir.join("design-spec.json");
        let retired = state_dir.join("retired.json");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(workspace.join("page.js"), "one").unwrap();
        fs::write(&draft, "draft-v1").unwrap();
        fs::write(&retired, "gone in the current schema").unwrap();
        let legacy = seed_legacy_checkpoint(&workspace, 1_000);

        let store = AppCheckpointStore::new(&layout);
        fs::remove_file(&retired).unwrap();
        fs::write(&draft, "draft-v2").unwrap();
        store
            .create(AppCheckpointKind::GenerationValidated, "second", 2_000)
            .unwrap();

        store.restore(&legacy, 3_000).unwrap();

        assert_eq!(fs::read_to_string(&draft).unwrap(), "draft-v2");
        assert!(!retired.exists(), "the reset must not resurrect it");
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_rejects_workspace_symlinks() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        let workspace = root.path().join(layout.workspace_rel());
        symlink("/tmp", workspace.join("escape")).unwrap();
        let error = AppCheckpointStore::new(&layout)
            .create(AppCheckpointKind::UserApproved, "unsafe", 1_000)
            .unwrap_err();
        assert_eq!(error.code(), crate::error::AppErrorCode::InvalidRequest);
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_rejects_gitdir_indirection() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        let workspace = root.path().join(layout.workspace_rel());
        symlink("/tmp", workspace.join(".git")).unwrap();
        let error = AppCheckpointStore::new(&layout)
            .create(AppCheckpointKind::UserApproved, "unsafe", 1_000)
            .unwrap_err();
        assert_eq!(error.code(), crate::error::AppErrorCode::InvalidRequest);
    }

    #[test]
    fn list_survives_head_moving_backwards() {
        let root = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        let workspace = root.path().join(layout.workspace_rel());
        let store = AppCheckpointStore::new(&layout);
        fs::write(workspace.join("page.js"), "one").unwrap();
        let first = store
            .create(AppCheckpointKind::ScaffoldCreated, "first", 1_000)
            .unwrap();
        fs::write(workspace.join("page.js"), "two").unwrap();
        store
            .create(AppCheckpointKind::GenerationValidated, "second", 2_000)
            .unwrap();
        store.restore(&first.id, 3_000).unwrap();
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 3);
        assert!(listed.iter().any(|checkpoint| checkpoint.id == first.id));
    }

    #[test]
    fn invalid_checkpoint_id_is_rejected_before_git_lookup() {
        let root = tempfile::tempdir().unwrap();
        let layout = layout(root.path());
        let error = AppCheckpointStore::new(&layout)
            .restore("../HEAD", 1_000)
            .unwrap_err();
        assert_eq!(error.code(), crate::error::AppErrorCode::InvalidRequest);
    }

    #[test]
    fn kinds_round_trip() {
        for kind in [
            AppCheckpointKind::ScaffoldCreated,
            AppCheckpointKind::GenerationValidated,
            AppCheckpointKind::PreviewApproved,
            AppCheckpointKind::UserApproved,
            AppCheckpointKind::PreRestore,
        ] {
            assert_eq!(parse_kind(kind_name(kind)), Some(kind));
        }
    }
}
