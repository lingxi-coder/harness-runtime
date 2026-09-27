//! Git operations — operation enum → `git2` calls (filled by Tasks 4-6/8).
//!
//! This module hosts the host-testable, deterministic `git2`-backed
//! implementations of each supported operation:
//!
//! - Task 4: workspace-anchored `open_repo` + path-escape validation +
//!   `GitOpError`.
//! - Task 5: read operations (status / diff / log / show / `branch_list`).
//! - Task 6: local write operations (add / commit / `branch_create` / checkout
//!   / merge fast-forward).
//! - Task 8: network operations (clone / fetch / pull) with the in-process
//!   credential callback + CA wiring.
//!
//! Tasks 4-6 are implemented (repo open + read + local-write ops); Task 8 adds
//! the network ops (clone/fetch/pull) with the in-process token credential
//! callback + CA wiring (see [`crate::auth`]). `GitTool::call` dispatches the
//! local ops and the network ops here.

use std::path::Path;

use serde::Serialize;
use telemetry::otel;
use thiserror::Error;

/// Upper bound on the number of UTF-8 characters of unified-diff text returned
/// by [`diff`] / [`show`]. libgit2 patches over an entire dirty tree can be
/// arbitrarily large; the model-facing tool truncates past this so a single
/// `Git` call can never blow the tool-result budget. When truncation occurs a
/// trailing `\n... [diff truncated]\n` marker is appended.
pub const GIT_DIFF_MAX_CHARS: usize = 100_000;

/// Default cap on the number of commits [`log`] walks back from HEAD when the
/// caller does not request an explicit limit.
pub const GIT_LOG_DEFAULT_MAX: usize = 50;

/// One entry of a `status` listing: a path and its human-readable status flags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitStatusEntry {
    /// Path relative to the repository workdir.
    pub path: String,
    /// Status flags as lowercase tokens, e.g. `["wt_modified"]` or
    /// `["index_new"]`. A single path may carry several flags at once
    /// (staged + unstaged changes).
    pub status: Vec<String>,
}

/// Metadata for a single commit, as surfaced by `log` / `show`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitCommitInfo {
    /// Full commit OID, hex-encoded.
    pub oid: String,
    /// First line of the commit message (may be empty).
    pub summary: String,
    /// Author identity rendered as `Name <email>`.
    pub author: String,
    /// Author time as a Unix timestamp (seconds).
    pub time: i64,
}

/// One entry of a `branch_list` listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitBranchInfo {
    /// Branch short name (e.g. `main`, `origin/main`).
    pub name: String,
    /// Whether this branch is the one HEAD points at.
    pub is_head: bool,
    /// Whether this is a remote-tracking branch.
    pub is_remote: bool,
}

/// Unified-diff payload returned by `diff` / `show`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitDiff {
    /// The unified-diff text (patch format), possibly truncated.
    pub patch: String,
    /// Whether [`GitDiff::patch`] was truncated at [`GIT_DIFF_MAX_CHARS`].
    pub truncated: bool,
}

/// Errors returned by git operations.
#[derive(Debug, Error)]
pub enum GitOpError {
    /// Requested path does not exist or cannot be canonicalized.
    #[error("not found: {0}")]
    NotFound(String),

    /// Requested path escapes the workspace root (symlink or `..` traversal).
    #[error("path escape: {0}")]
    Escape(String),

    /// A libgit2 error.
    #[error("libgit2: {0}")]
    Libgit2(String),

    /// The repository has uncommitted changes that would be clobbered.
    #[error("dirty worktree: {0}")]
    Dirty(String),

    /// The requested merge/pull cannot be resolved as a fast-forward.
    #[error("non-fast-forward: {0}")]
    NonFastForward(String),

    /// The caller supplied invalid or inconsistent parameters.
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

impl GitOpError {
    /// Map a `git2::Error` into a [`GitOpError::Libgit2`].
    #[must_use]
    pub fn from_git2(e: &git2::Error) -> Self {
        Self::Libgit2(e.message().to_owned())
    }
}

/// Open a git repository at `repo_rel` (a path relative to `workspace_root`),
/// enforcing that the canonicalized repo path stays inside the canonicalized
/// workspace root.
///
/// This mirrors the containment check in
/// `AndroidMinijailSandbox::resolve_cwd` (canonicalize + `starts_with`), so
/// symlink escapes and `..` traversals are rejected before `git2` ever touches
/// the path.
///
/// # Errors
///
/// - [`GitOpError::NotFound`] — `workspace_root` or the joined path cannot be
///   canonicalized (directory does not exist).
/// - [`GitOpError::Escape`] — the canonicalized repo path lies outside the
///   canonicalized workspace root.
/// - [`GitOpError::Libgit2`] — `git2::Repository::open` failed (e.g. not a
///   git repo).
pub fn open_repo(workspace_root: &Path, repo_rel: &str) -> Result<git2::Repository, GitOpError> {
    // Canonicalize the workspace root first so we have a clean baseline.
    let canonical_root = workspace_root.canonicalize().map_err(|e| {
        GitOpError::NotFound(format!("workspace root {}: {e}", workspace_root.display()))
    })?;

    // Build the requested repo path and canonicalize it.
    let requested = canonical_root.join(repo_rel);
    let canonical_repo = requested
        .canonicalize()
        .map_err(|e| GitOpError::NotFound(format!("{}: {e}", requested.display())))?;

    // Containment check — mirrors `AndroidMinijailSandbox::resolve_cwd`.
    if !canonical_repo.starts_with(&canonical_root) {
        return Err(GitOpError::Escape(canonical_repo.display().to_string()));
    }

    git2::Repository::open(&canonical_repo).map_err(|e| GitOpError::from_git2(&e))
}

/// Render a [`git2::Status`] bitset as lowercase tokens (`wt_modified`,
/// `index_new`, …). A single path can carry several flags (staged + unstaged).
fn status_flags(s: git2::Status) -> Vec<String> {
    use git2::Status;
    let mut out = Vec::new();
    let mut push = |flag: Status, name: &str| {
        if s.contains(flag) {
            out.push(name.to_owned());
        }
    };
    push(Status::INDEX_NEW, "index_new");
    push(Status::INDEX_MODIFIED, "index_modified");
    push(Status::INDEX_DELETED, "index_deleted");
    push(Status::INDEX_RENAMED, "index_renamed");
    push(Status::INDEX_TYPECHANGE, "index_typechange");
    push(Status::WT_NEW, "wt_new");
    push(Status::WT_MODIFIED, "wt_modified");
    push(Status::WT_DELETED, "wt_deleted");
    push(Status::WT_TYPECHANGE, "wt_typechange");
    push(Status::WT_RENAMED, "wt_renamed");
    push(Status::CONFLICTED, "conflicted");
    push(Status::IGNORED, "ignored");
    out
}

/// List the working-tree + index status of every changed path, including
/// untracked files.
///
/// # Errors
///
/// [`GitOpError::Libgit2`] if libgit2 cannot compute the status (e.g. a bare
/// repository, which has no working tree).
pub fn status(repo: &git2::Repository) -> Result<Vec<GitStatusEntry>, GitOpError> {
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true).include_ignored(false);
    let statuses = repo
        .statuses(Some(&mut opts))
        .map_err(|e| GitOpError::from_git2(&e))?;
    let mut entries = Vec::with_capacity(statuses.len());
    for entry in statuses.iter() {
        let path = entry.path().unwrap_or_default().to_owned();
        entries.push(GitStatusEntry {
            path,
            status: status_flags(entry.status()),
        });
    }
    Ok(entries)
}

/// Build a [`GitCommitInfo`] from a commit.
fn commit_info(commit: &git2::Commit<'_>) -> GitCommitInfo {
    let author = commit.author();
    let name = author.name().unwrap_or("");
    let email = author.email().unwrap_or("");
    GitCommitInfo {
        oid: commit.id().to_string(),
        summary: commit
            .summary()
            .ok()
            .flatten()
            .unwrap_or_default()
            .to_owned(),
        author: format!("{name} <{email}>"),
        time: author.when().seconds(),
    }
}

/// Walk commit history from HEAD, newest-first, capped at `max`
/// (default [`GIT_LOG_DEFAULT_MAX`]).
///
/// # Errors
///
/// [`GitOpError::Libgit2`] if HEAD is unborn or the revwalk fails.
pub fn log(repo: &git2::Repository, max: Option<usize>) -> Result<Vec<GitCommitInfo>, GitOpError> {
    let cap = max.unwrap_or(GIT_LOG_DEFAULT_MAX);
    let mut walk = repo.revwalk().map_err(|e| GitOpError::from_git2(&e))?;
    walk.set_sorting(git2::Sort::TIME | git2::Sort::TOPOLOGICAL)
        .map_err(|e| GitOpError::from_git2(&e))?;
    walk.push_head().map_err(|e| GitOpError::from_git2(&e))?;

    let mut out = Vec::new();
    for oid in walk.take(cap) {
        let oid = oid.map_err(|e| GitOpError::from_git2(&e))?;
        let commit = repo
            .find_commit(oid)
            .map_err(|e| GitOpError::from_git2(&e))?;
        out.push(commit_info(&commit));
    }
    Ok(out)
}

/// Render a [`git2::Diff`] to unified-patch text, truncating at
/// [`GIT_DIFF_MAX_CHARS`].
fn diff_to_text(diff: &git2::Diff<'_>) -> Result<GitDiff, GitOpError> {
    let mut patch = String::new();
    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        // Prefix bytes (`+`/`-`/` `) only for context/add/delete lines; libgit2
        // already embeds the prefix for file/hunk headers.
        match line.origin() {
            '+' | '-' | ' ' => patch.push(line.origin()),
            _ => {}
        }
        patch.push_str(&String::from_utf8_lossy(line.content()));
        true
    })
    .map_err(|e| GitOpError::from_git2(&e))?;

    if patch.chars().count() > GIT_DIFF_MAX_CHARS {
        let truncated: String = patch.chars().take(GIT_DIFF_MAX_CHARS).collect();
        Ok(GitDiff {
            patch: format!("{truncated}\n... [diff truncated]\n"),
            truncated: true,
        })
    } else {
        Ok(GitDiff {
            patch,
            truncated: false,
        })
    }
}

/// Unified diff of the working tree against HEAD (staged + unstaged changes),
/// truncated at [`GIT_DIFF_MAX_CHARS`].
///
/// Unlike desktop `git diff` (which hides untracked files by default), this
/// intentionally INCLUDES untracked files via `include_untracked` /
/// `recurse_untracked_dirs`, so brand-new files appear in the diff's delta list
/// — one call surfaces new files on a constrained device. Note: this does NOT
/// set `show_untracked_content`, so an untracked file's content is detected but
/// not printed as `+` lines in the patch body. See the matching note in the
/// tool's `diff` prompt description.
///
/// # Errors
///
/// [`GitOpError::Libgit2`] if HEAD or the diff cannot be resolved.
pub fn diff(repo: &git2::Repository) -> Result<GitDiff, GitOpError> {
    let head_tree = repo
        .head()
        .and_then(|h| h.peel_to_tree())
        .map_err(|e| GitOpError::from_git2(&e))?;
    let mut opts = git2::DiffOptions::new();
    opts.include_untracked(true).recurse_untracked_dirs(true);
    let diff = repo
        .diff_tree_to_workdir_with_index(Some(&head_tree), Some(&mut opts))
        .map_err(|e| GitOpError::from_git2(&e))?;
    diff_to_text(&diff)
}

/// Resolve `rev` and, when it is (or peels to) a commit, return its metadata
/// plus the unified diff against its first parent (or against the empty tree
/// for a root commit), truncated at [`GIT_DIFF_MAX_CHARS`].
///
/// # Errors
///
/// - [`GitOpError::Libgit2`] if `rev` cannot be parsed or peeled.
/// - [`GitOpError::InvalidInput`] if `rev` does not resolve to a commit.
pub fn show(repo: &git2::Repository, rev: &str) -> Result<(GitCommitInfo, GitDiff), GitOpError> {
    let obj = repo
        .revparse_single(rev)
        .map_err(|e| GitOpError::from_git2(&e))?;
    let commit = obj
        .peel_to_commit()
        .map_err(|_| GitOpError::InvalidInput(format!("rev {rev} is not a commit")))?;

    let new_tree = commit.tree().map_err(|e| GitOpError::from_git2(&e))?;
    let parent_tree = match commit.parent(0) {
        Ok(parent) => Some(parent.tree().map_err(|e| GitOpError::from_git2(&e))?),
        Err(_) => None, // root commit — diff against the empty tree.
    };
    let diff = repo
        .diff_tree_to_tree(parent_tree.as_ref(), Some(&new_tree), None)
        .map_err(|e| GitOpError::from_git2(&e))?;
    Ok((commit_info(&commit), diff_to_text(&diff)?))
}

/// List every local + remote-tracking branch.
///
/// # Errors
///
/// [`GitOpError::Libgit2`] if the branch iterator fails.
pub fn branch_list(repo: &git2::Repository) -> Result<Vec<GitBranchInfo>, GitOpError> {
    let branches = repo.branches(None).map_err(|e| GitOpError::from_git2(&e))?;
    let mut out = Vec::new();
    for item in branches {
        let (branch, kind) = item.map_err(|e| GitOpError::from_git2(&e))?;
        let name = branch
            .name()
            .map_err(|e| GitOpError::from_git2(&e))?
            .unwrap_or("")
            .to_owned();
        out.push(GitBranchInfo {
            name,
            is_head: branch.is_head(),
            is_remote: matches!(kind, git2::BranchType::Remote),
        });
    }
    Ok(out)
}

/// Default committer identity used when the repository config supplies neither
/// `user.name` nor `user.email`. Only the repository's own config is consulted
/// (never the global / system / env gitconfig), so write ops are deterministic
/// on a fresh app-private clone.
pub const DEFAULT_SIGNATURE_NAME: &str = "LingXi";
/// Default committer email paired with [`DEFAULT_SIGNATURE_NAME`].
pub const DEFAULT_SIGNATURE_EMAIL: &str = "noreply@lingxi";

/// Result of an [`add`] call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitAddResult {
    /// Total entries in the index after staging (not the delta added by this
    /// call — the index includes all previously-tracked paths).
    pub index_entries: usize,
}

/// Result of a [`commit`] call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitCommitResult {
    /// Full OID of the newly created commit, hex-encoded.
    pub oid: String,
}

/// Result of a [`branch_create`] call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitBranchCreateResult {
    /// The branch that was created.
    pub branch: String,
}

/// Result of a [`checkout`] call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitCheckoutResult {
    /// The target that HEAD now points at (branch name or detached OID).
    pub target: String,
}

/// Result of a [`merge`] call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitMergeResult {
    /// Whether the merge was a fast-forward (the only kind v1 performs).
    pub fast_forward: bool,
    /// OID HEAD now points at after the fast-forward.
    pub oid: String,
}

/// Resolve the committer/author signature for a write op.
///
/// Reads ONLY the repository's own config (`user.name` / `user.email`); the
/// global, system, and `$GIT_*` env gitconfig are never consulted. When either
/// field is missing, the fixed [`DEFAULT_SIGNATURE_NAME`] /
/// [`DEFAULT_SIGNATURE_EMAIL`] default is used.
fn resolve_signature(repo: &git2::Repository) -> Result<git2::Signature<'static>, GitOpError> {
    // Open ONLY the repository-local config level: never the global / system /
    // `$GIT_*` env gitconfig. `open_level(Local)` may fail (e.g. no local
    // config file yet) — in that case treat every field as absent and fall back
    // to the fixed default, so the result is deterministic on a fresh clone.
    let local = repo
        .config()
        .and_then(|cfg| cfg.open_level(git2::ConfigLevel::Local))
        .ok();
    let get = |key: &str| -> Option<String> {
        local
            .as_ref()
            .and_then(|cfg| cfg.get_string(key).ok())
            .filter(|s| !s.is_empty())
    };
    let name = get("user.name").unwrap_or_else(|| DEFAULT_SIGNATURE_NAME.to_owned());
    let email = get("user.email").unwrap_or_else(|| DEFAULT_SIGNATURE_EMAIL.to_owned());
    git2::Signature::now(&name, &email).map_err(|e| GitOpError::from_git2(&e))
}

/// Stage `paths` into the index (defaulting to all changes, `["*"]`, when
/// empty), then persist the index.
///
/// # Errors
///
/// [`GitOpError::Libgit2`] if the index cannot be opened, the add fails, or the
/// write fails.
pub fn add(repo: &git2::Repository, paths: &[String]) -> Result<GitAddResult, GitOpError> {
    let mut index = repo.index().map_err(|e| GitOpError::from_git2(&e))?;
    let specs: Vec<&str> = if paths.is_empty() {
        vec!["*"]
    } else {
        paths.iter().map(String::as_str).collect()
    };
    index
        .add_all(specs.iter(), git2::IndexAddOption::DEFAULT, None)
        .map_err(|e| GitOpError::from_git2(&e))?;
    index.write().map_err(|e| GitOpError::from_git2(&e))?;
    Ok(GitAddResult {
        index_entries: index.len(),
    })
}

/// Build a tree from the current index and commit it on top of HEAD (or as a
/// root commit when HEAD is unborn). The committer/author identity is resolved
/// by [`resolve_signature`].
///
/// # Errors
///
/// - [`GitOpError::InvalidInput`] if `message` is empty.
/// - [`GitOpError::Libgit2`] if the index/tree/commit cannot be built.
pub fn commit(repo: &git2::Repository, message: &str) -> Result<GitCommitResult, GitOpError> {
    if message.is_empty() {
        return Err(GitOpError::InvalidInput("commit message is empty".into()));
    }
    let sig = resolve_signature(repo)?;
    let mut index = repo.index().map_err(|e| GitOpError::from_git2(&e))?;
    let tree_oid = index.write_tree().map_err(|e| GitOpError::from_git2(&e))?;
    let tree = repo
        .find_tree(tree_oid)
        .map_err(|e| GitOpError::from_git2(&e))?;

    // Parent is the current HEAD commit, if HEAD is born.
    let parent = match repo.head() {
        Ok(head) => Some(
            head.peel_to_commit()
                .map_err(|e| GitOpError::from_git2(&e))?,
        ),
        Err(_) => None, // unborn HEAD — root commit
    };
    let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();

    let oid = repo
        .commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
        .map_err(|e| GitOpError::from_git2(&e))?;
    otel::record_counter(otel::metrics::COMMIT_COUNT, 1.0, &otel::Attributes::new());
    Ok(GitCommitResult {
        oid: oid.to_string(),
    })
}

/// Create a new branch `name` pointing at the current HEAD commit.
///
/// # Errors
///
/// [`GitOpError::Libgit2`] if HEAD is unborn or the branch already exists.
pub fn branch_create(
    repo: &git2::Repository,
    name: &str,
) -> Result<GitBranchCreateResult, GitOpError> {
    if name.is_empty() {
        return Err(GitOpError::InvalidInput("branch name is empty".into()));
    }
    let head_commit = repo
        .head()
        .and_then(|h| h.peel_to_commit())
        .map_err(|e| GitOpError::from_git2(&e))?;
    repo.branch(name, &head_commit, false)
        .map_err(|e| GitOpError::from_git2(&e))?;
    Ok(GitBranchCreateResult {
        branch: name.to_owned(),
    })
}

/// Return `true` when the working tree + index have no changes that a checkout
/// could clobber (untracked files are ignored — they are never overwritten by a
/// safe checkout).
fn worktree_is_clean(repo: &git2::Repository) -> Result<bool, GitOpError> {
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(false).include_ignored(false);
    let statuses = repo
        .statuses(Some(&mut opts))
        .map_err(|e| GitOpError::from_git2(&e))?;
    Ok(statuses.is_empty())
}

/// Switch HEAD + the working tree to `target` (a branch name or revision).
///
/// Refuses to run on a dirty working tree ([`GitOpError::Dirty`]) so no
/// uncommitted change can be silently lost. The checkout itself is performed in
/// safe mode (NOT force), so libgit2 will also refuse rather than overwrite.
///
/// # Errors
///
/// - [`GitOpError::Dirty`] if the working tree has staged/unstaged changes.
/// - [`GitOpError::NotFound`] if `target` cannot be resolved.
/// - [`GitOpError::Libgit2`] if the checkout/`set_head` fails.
pub fn checkout(repo: &git2::Repository, target: &str) -> Result<GitCheckoutResult, GitOpError> {
    if target.is_empty() {
        return Err(GitOpError::InvalidInput("checkout target is empty".into()));
    }
    if !worktree_is_clean(repo)? {
        return Err(GitOpError::Dirty(format!(
            "working tree has uncommitted changes; refusing to checkout {target}"
        )));
    }

    // Resolve the target object and the ref name (if it is a branch) so we can
    // set a symbolic HEAD where possible (otherwise detach to the commit OID).
    let obj = repo
        .revparse_single(target)
        .map_err(|_| GitOpError::NotFound(format!("checkout target {target}")))?;
    let commit = obj
        .peel_to_commit()
        .map_err(|e| GitOpError::from_git2(&e))?;
    let tree = commit.tree().map_err(|e| GitOpError::from_git2(&e))?;

    // Safe checkout (no force): libgit2 refuses to overwrite local modifications.
    let mut co = git2::build::CheckoutBuilder::new();
    co.safe();
    repo.checkout_tree(tree.as_object(), Some(&mut co))
        .map_err(|e| GitOpError::from_git2(&e))?;

    // Prefer a symbolic HEAD when the target is a local branch.
    let head_target = if let Ok(branch) = repo.find_branch(target, git2::BranchType::Local) {
        let refname = branch
            .get()
            .name()
            .map_err(|e| GitOpError::from_git2(&e))?
            .to_owned();
        repo.set_head(&refname)
            .map_err(|e| GitOpError::from_git2(&e))?;
        target.to_owned()
    } else {
        repo.set_head_detached(commit.id())
            .map_err(|e| GitOpError::from_git2(&e))?;
        commit.id().to_string()
    };

    Ok(GitCheckoutResult {
        target: head_target,
    })
}

/// Fast-forward HEAD to `source` when possible; refuse otherwise.
///
/// Runs `merge_analysis` against `source`'s commit. Only an
/// `ANALYSIS_FASTFORWARD` result proceeds (move HEAD + `checkout_tree`); a
/// diverged history yields [`GitOpError::NonFastForward`] with NO 3-way merge
/// attempted and NO conflict markers written. An already-up-to-date HEAD is a
/// no-op fast-forward.
///
/// # Errors
///
/// - [`GitOpError::NotFound`] if `source` cannot be resolved.
/// - [`GitOpError::NonFastForward`] if the merge is not a fast-forward.
/// - [`GitOpError::Libgit2`] on any libgit2 failure.
pub fn merge(repo: &git2::Repository, source: &str) -> Result<GitMergeResult, GitOpError> {
    if source.is_empty() {
        return Err(GitOpError::InvalidInput("merge source is empty".into()));
    }
    let source_obj = repo
        .revparse_single(source)
        .map_err(|_| GitOpError::NotFound(format!("merge source {source}")))?;
    let source_commit = source_obj
        .peel_to_commit()
        .map_err(|e| GitOpError::from_git2(&e))?;
    let annotated = repo
        .find_annotated_commit(source_commit.id())
        .map_err(|e| GitOpError::from_git2(&e))?;

    let (analysis, _pref) = repo
        .merge_analysis(&[&annotated])
        .map_err(|e| GitOpError::from_git2(&e))?;

    if analysis.is_up_to_date() {
        // Already contains `source` — nothing to do; report the current HEAD.
        let head = repo
            .head()
            .and_then(|h| h.peel_to_commit())
            .map_err(|e| GitOpError::from_git2(&e))?;
        return Ok(GitMergeResult {
            fast_forward: true,
            oid: head.id().to_string(),
        });
    }

    if !analysis.is_fast_forward() {
        return Err(GitOpError::NonFastForward(format!(
            "merge of {source} is not a fast-forward; refusing (no 3-way merge in v1)"
        )));
    }

    // Fast-forward: move the current branch ref to the source commit and sync
    // the working tree.
    let target_oid = source_commit.id();
    let tree = source_commit
        .tree()
        .map_err(|e| GitOpError::from_git2(&e))?;
    let mut co = git2::build::CheckoutBuilder::new();
    co.safe();
    repo.checkout_tree(tree.as_object(), Some(&mut co))
        .map_err(|e| GitOpError::from_git2(&e))?;

    // Update the ref HEAD points at (or detached HEAD) to the target commit.
    match repo.head() {
        Ok(head) if head.is_branch() => {
            let refname = head
                .name()
                .map_err(|e| GitOpError::from_git2(&e))?
                .to_owned();
            let mut reference = repo
                .find_reference(&refname)
                .map_err(|e| GitOpError::from_git2(&e))?;
            reference
                .set_target(target_oid, "merge: fast-forward")
                .map_err(|e| GitOpError::from_git2(&e))?;
        }
        _ => {
            repo.set_head_detached(target_oid)
                .map_err(|e| GitOpError::from_git2(&e))?;
        }
    }

    Ok(GitMergeResult {
        fast_forward: true,
        oid: target_oid.to_string(),
    })
}

// =============================================================================
// Network operations (Task 8) — clone / fetch / pull
// =============================================================================

/// Per-operation network configuration carried by `GitTool::call` into the
/// network ops. The `provider` yields the HTTPS token / SSH passphrase in-memory
/// on demand (never written to disk or a child-process env); `ca_dir` points
/// libgit2's TLS backend at a CA-certificate directory (Android system cacerts).
///
/// This rides a SEPARATE secret seam from the public `AndroidGitToolCtx` so the
/// secrets never enter the broadly-cloned public tool context — see
/// `tool-api`'s `BuiltinToolContext.android_git_secret`.
#[derive(Clone, Default)]
pub struct GitNetConfig {
    /// Per-op credential provider supplying the HTTPS token (used as the password
    /// in the credential callback) and SSH passphrase on demand, or `None` for
    /// anonymous / public remotes. Secrets are fetched lazily, never held
    /// resident; never logged or persisted.
    pub provider: Option<std::sync::Arc<dyn tool_api::GitCredentialProvider>>,
    /// CA-certificate directory for TLS verification, or `None` to use the
    /// libgit2/OpenSSL defaults (the host `file://` tests need none).
    pub ca_dir: Option<String>,
    /// SSH-key authentication material + pinned host keys (spec §G7), or `None`
    /// when SSH is not configured (HTTPS-only). When `None`, an SSH remote URL
    /// is rejected by [`ssh_allowed`]; when `Some`, SSH clone/fetch/pull/push
    /// work through [`crate::auth::make_network_callbacks`]. Host-supplied; never
    /// model-supplied. Never logged (the key path/host keys are non-secret; the
    /// passphrase is masked by `SshConfig`'s carrier).
    pub ssh: Option<crate::auth::SshConfig>,
}

// Manual redacting Debug — the `provider` (and the secrets it can yield) must
// never reach a log line, mirroring `tool_api::AndroidGitSecret`. (`Arc<dyn T>`
// is not `Debug` anyway; the struct derives only `Clone`, not `Debug`, so this
// is the sole Debug path.)
impl std::fmt::Debug for GitNetConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitNetConfig")
            .field("provider", &self.provider.as_ref().map(|_| "<provider>"))
            .field("ca_dir", &self.ca_dir)
            // `SshConfig` carries key material; never print its contents — only
            // whether SSH is configured.
            .field("ssh", &self.ssh.as_ref().map(|_| "<configured>"))
            .finish()
    }
}

/// Result of a [`clone`] call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitCloneResult {
    /// Absolute path of the freshly cloned working tree.
    pub path: String,
    /// OID of the cloned HEAD commit, hex-encoded.
    pub head: String,
}

/// Result of a [`fetch`] call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitFetchResult {
    /// The remote that was fetched from.
    pub remote: String,
}

/// Result of a [`pull`] call (fetch + fast-forward merge).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitPullResult {
    /// Whether the merge half was a fast-forward (the only kind v1 performs).
    pub fast_forward: bool,
    /// OID HEAD points at after the pull.
    pub oid: String,
}

/// Result of a [`push`] call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitPushResult {
    /// The remote pushed to.
    pub remote: String,
    /// The branch short name pushed.
    pub branch: String,
    /// OID the local branch tip points at (now also on the remote).
    pub pushed_oid: String,
    /// Whether this push set the branch's upstream (true only on first push of
    /// a branch that had none).
    pub set_upstream: bool,
}

/// Is `url` an SSH-transport remote (`ssh://…`, `git+ssh://…`, `git@…`, or the
/// generic `user@host:path` scp syntax)?
///
/// Shared by [`ssh_allowed`] so the SSH-detection logic lives in exactly one
/// place.
fn is_ssh_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    // scp-like `git@host:path` or explicit `ssh://` / `git+ssh://`.
    lower.starts_with("ssh://")
        || lower.starts_with("git+ssh://")
        || lower.starts_with("git@")
        // generic `user@host:path` scp syntax (has `@` and a `:` before any `/`).
        || (lower.contains('@')
            && !lower.contains("://")
            && lower
                .split_once(':')
                .is_some_and(|(left, _)| left.contains('@')))
}

/// Gate an SSH remote URL on whether SSH credentials are configured (spec §G7).
///
/// An SSH URL is allowed only when `ssh` is `Some` (key material + pinned host
/// keys supplied by the host); otherwise it is rejected with the named
/// SSH-unsupported error. Non-SSH (HTTPS / `file://`) URLs are always allowed,
/// regardless of `ssh`.
///
/// # Errors
///
/// [`GitOpError::InvalidInput`] naming ssh/HTTPS when `url` is an SSH remote and
/// no SSH config is present.
pub(crate) fn ssh_allowed(
    url: &str,
    ssh: Option<&crate::auth::SshConfig>,
) -> Result<(), GitOpError> {
    if is_ssh_url(url) && ssh.is_none() {
        return Err(GitOpError::InvalidInput(
            "ssh URLs are not supported without SSH credentials configured; use HTTPS".into(),
        ));
    }
    Ok(())
}

/// Gate a remote URL on its transport scheme, enforcing the tool's documented
/// **HTTPS-ONLY** contract as a *positive* allowlist rather than an SSH-only
/// denylist (audit finding: an SSH-only gate let `http://` and `git://` through).
///
/// - `https://` — always allowed (TLS + host credential over an encrypted
///   channel).
/// - SSH remotes (`ssh://`, `git@…`, scp syntax) — delegated to [`ssh_allowed`]
///   (allowed only when SSH credentials are configured).
/// - `file://` — allowed ONLY in test builds (the `file://` bare-remote suite);
///   rejected in production so a model-supplied local path / `file://` cannot be
///   used to read arbitrary app-UID files into the workspace.
/// - everything else — rejected. In particular `http://` would send the
///   host-supplied PAT as cleartext HTTP Basic auth (token exfil on hostile
///   Wi-Fi) and `git://` is unauthenticated/MITM-able; both contradict the
///   advertised HTTPS-only policy.
///
/// Called on the model-supplied `repo_url` at clone and on the *resolved* remote
/// URL at fetch/push, so a remote configured out of band is re-checked.
///
/// # Errors
///
/// [`GitOpError::InvalidInput`] naming HTTPS for any non-allowed scheme.
pub(crate) fn transport_allowed(
    url: &str,
    ssh: Option<&crate::auth::SshConfig>,
) -> Result<(), GitOpError> {
    if is_ssh_url(url) {
        return ssh_allowed(url, ssh);
    }
    let lower = url.trim().to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Ok(());
    }
    #[cfg(test)]
    if lower.starts_with("file://") {
        return Ok(());
    }
    Err(GitOpError::InvalidInput(format!(
        "remote `{url}` is not allowed; only HTTPS (https://) remotes are supported"
    )))
}

/// Clone `repo_url` into `dest_rel` (relative to `workspace_root`).
///
/// SSH URLs are rejected (G7). The destination is validated to stay inside the
/// canonicalized workspace root — since `dest` does not exist yet, its PARENT
/// is canonicalized and checked (mirrors [`open_repo`]'s containment guard).
/// The CA location is set first, then `RepoBuilder` clones with the
/// token-bearing fetch options.
///
/// # Errors
///
/// - [`GitOpError::InvalidInput`] — SSH URL, or empty `repo_url`.
/// - [`GitOpError::Escape`] — `dest_rel` resolves outside the workspace root.
/// - [`GitOpError::NotFound`] — the workspace root / dest parent cannot be
///   canonicalized.
/// - [`GitOpError::Libgit2`] — the clone (network / TLS / auth) failed.
pub fn clone(
    net: &GitNetConfig,
    workspace_root: &Path,
    repo_url: &str,
    dest_rel: &str,
) -> Result<GitCloneResult, GitOpError> {
    if repo_url.is_empty() {
        return Err(GitOpError::InvalidInput("clone requires `repo_url`".into()));
    }
    transport_allowed(repo_url, net.ssh.as_ref())?;

    // Resolve + validate the destination. `dest` itself does not exist yet, so
    // canonicalize its PARENT and require that to be inside the workspace root,
    // then re-attach the final component.
    let canonical_root = workspace_root.canonicalize().map_err(|e| {
        GitOpError::NotFound(format!("workspace root {}: {e}", workspace_root.display()))
    })?;
    let requested = canonical_root.join(dest_rel);
    let parent = requested
        .parent()
        .ok_or_else(|| GitOpError::InvalidInput(format!("dest {dest_rel} has no parent")))?;
    let file_name = requested.file_name().ok_or_else(|| {
        GitOpError::InvalidInput(format!("dest {dest_rel} has no final component"))
    })?;
    let canonical_parent = parent
        .canonicalize()
        .map_err(|e| GitOpError::NotFound(format!("{}: {e}", parent.display())))?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(GitOpError::Escape(canonical_parent.display().to_string()));
    }
    let dest = canonical_parent.join(file_name);

    // CA wiring first, then clone with the unified network callbacks (HTTPS
    // token and/or SSH key + strict host-key check).
    crate::auth::set_ca_location(net.ca_dir.as_deref())?;
    crate::auth::ensure_ssh_homedir(net.ssh.as_ref())?;
    let fetch_opts = crate::auth::make_fetch_options_net(&crate::auth::NetCallbacks {
        provider: net.provider.as_deref(),
        ssh: net.ssh.as_ref(),
    });
    let mut builder = git2::build::RepoBuilder::new();
    builder.fetch_options(fetch_opts);
    let repo = builder
        .clone(repo_url, &dest)
        .map_err(|e| GitOpError::from_git2(&e))?;

    let head = repo
        .head()
        .and_then(|h| h.peel_to_commit())
        .map_err(|e| GitOpError::from_git2(&e))?;
    Ok(GitCloneResult {
        path: dest.display().to_string(),
        head: head.id().to_string(),
    })
}

/// Fetch from `remote_name` (default `origin`) using the token-bearing fetch
/// options. The remote's configured URL is also checked against the SSH guard.
///
/// # Errors
///
/// - [`GitOpError::InvalidInput`] — the remote URL is an SSH URL.
/// - [`GitOpError::Libgit2`] — the remote is missing or the fetch failed.
pub fn fetch(
    net: &GitNetConfig,
    repo: &git2::Repository,
    remote_name: &str,
) -> Result<GitFetchResult, GitOpError> {
    let name = if remote_name.is_empty() {
        "origin"
    } else {
        remote_name
    };
    let mut remote = repo
        .find_remote(name)
        .map_err(|e| GitOpError::from_git2(&e))?;
    if let Ok(url) = remote.url() {
        transport_allowed(url, net.ssh.as_ref())?;
    }
    crate::auth::set_ca_location(net.ca_dir.as_deref())?;
    crate::auth::ensure_ssh_homedir(net.ssh.as_ref())?;
    let mut fetch_opts = crate::auth::make_fetch_options_net(&crate::auth::NetCallbacks {
        provider: net.provider.as_deref(),
        ssh: net.ssh.as_ref(),
    });
    // Empty refspec slice -> libgit2 uses the remote's configured default
    // refspecs (refs/heads/* -> refs/remotes/<remote>/*).
    let empty: [&str; 0] = [];
    remote
        .fetch(&empty, Some(&mut fetch_opts), None)
        .map_err(|e| GitOpError::from_git2(&e))?;
    Ok(GitFetchResult {
        remote: name.to_owned(),
    })
}

/// Pull: [`fetch`] then a **fast-forward-only** merge of the fetched
/// remote-tracking ref (`<remote>/<branch>`) into the current branch, reusing
/// [`merge`]'s ff logic. A diverged history yields [`GitOpError::NonFastForward`]
/// (no 3-way merge, no conflict markers).
///
/// `branch` defaults to the short name of the current HEAD branch when empty.
///
/// # Errors
///
/// - [`GitOpError::InvalidInput`] — the remote URL is SSH, or HEAD is detached
///   and no `branch` was supplied.
/// - [`GitOpError::NonFastForward`] — the merge is not a fast-forward.
/// - [`GitOpError::Libgit2`] — the fetch / ref resolution failed.
pub fn pull(
    net: &GitNetConfig,
    repo: &git2::Repository,
    remote_name: &str,
    branch: &str,
) -> Result<GitPullResult, GitOpError> {
    let remote = if remote_name.is_empty() {
        "origin"
    } else {
        remote_name
    };
    fetch(net, repo, remote)?;

    // Resolve the branch short name: explicit param, else current HEAD branch.
    let branch_name = if branch.is_empty() {
        let head = repo.head().map_err(|e| GitOpError::from_git2(&e))?;
        if !head.is_branch() {
            return Err(GitOpError::InvalidInput(
                "pull on a detached HEAD requires an explicit `branch`".into(),
            ));
        }
        head.shorthand()
            .map_err(|e| GitOpError::from_git2(&e))?
            .to_owned()
    } else {
        branch.to_owned()
    };

    // The fetched remote-tracking ref to fast-forward onto.
    let tracking = format!("refs/remotes/{remote}/{branch_name}");
    let merge_result = merge(repo, &tracking)?;
    Ok(GitPullResult {
        fast_forward: merge_result.fast_forward,
        oid: merge_result.oid,
    })
}

/// Push the current (or explicit) local branch to `remote_name` (default
/// `origin`) over the in-process token, **fast-forward-only**. On first push of
/// a branch with no upstream, write `branch.<b>.remote`/`.merge` (`git push -u`).
/// No force, no ref deletion, no tags.
///
/// libgit2 has TWO routes for a non-fast-forward rejection depending on the
/// transport: a per-ref status via the `push_update_reference` callback (HTTP
/// smart protocol), OR a top-level `remote.push` error whose message names the
/// "not present locally" / non-fast-forward condition (the local `file://`
/// transport). We install the callback AND inspect the top-level error message,
/// mapping either to [`GitOpError::NonFastForward`].
///
/// # Errors
///
/// - [`GitOpError::InvalidInput`] — SSH remote URL, or HEAD is detached and no
///   `branch` was supplied.
/// - [`GitOpError::NonFastForward`] — the remote rejected the ref (remote ahead).
/// - [`GitOpError::Libgit2`] — unknown remote / network / TLS / auth failure.
pub fn push(
    net: &GitNetConfig,
    repo: &git2::Repository,
    remote_name: &str,
    branch: &str,
) -> Result<GitPushResult, GitOpError> {
    let remote_name = if remote_name.is_empty() {
        "origin"
    } else {
        remote_name
    };

    let branch_name = if branch.is_empty() {
        let head = repo.head().map_err(|e| GitOpError::from_git2(&e))?;
        if !head.is_branch() {
            return Err(GitOpError::InvalidInput(
                "push on a detached HEAD requires an explicit `branch`".into(),
            ));
        }
        head.shorthand()
            .map_err(|e| GitOpError::from_git2(&e))?
            .to_owned()
    } else {
        branch.to_owned()
    };

    let mut remote = repo
        .find_remote(remote_name)
        .map_err(|e| GitOpError::from_git2(&e))?;
    if let Ok(url) = remote.url() {
        transport_allowed(url, net.ssh.as_ref())?;
    }

    crate::auth::set_ca_location(net.ca_dir.as_deref())?;
    crate::auth::ensure_ssh_homedir(net.ssh.as_ref())?;

    let rejection: std::rc::Rc<std::cell::RefCell<Option<String>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    // Unified network callbacks (HTTPS token and/or SSH key + strict host-key
    // check), with the existing non-ff `push_update_reference` capture ADDED on
    // top so push works over SSH while preserving the rejection detection.
    let mut callbacks = crate::auth::make_network_callbacks(&crate::auth::NetCallbacks {
        provider: net.provider.as_deref(),
        ssh: net.ssh.as_ref(),
    });
    {
        let rejection = std::rc::Rc::clone(&rejection);
        callbacks.push_update_reference(move |refname, status| {
            if let Some(msg) = status {
                *rejection.borrow_mut() = Some(format!("{refname}: {msg}"));
            }
            Ok(())
        });
    }
    let mut push_opts = git2::PushOptions::new();
    push_opts.remote_callbacks(callbacks);

    let refspec = format!("refs/heads/{branch_name}:refs/heads/{branch_name}");
    // libgit2 has TWO routes for a non-fast-forward rejection depending on the
    // transport: a per-ref status via `push_update_reference` (HTTP smart
    // protocol), OR a top-level `remote.push` error whose message names the
    // "not present locally" / non-fast-forward condition (the `file://` local
    // transport this test uses). Map BOTH to `NonFastForward`.
    remote
        .push(&[refspec.as_str()], Some(&mut push_opts))
        .map_err(|e| {
            let lower = e.message().to_ascii_lowercase();
            if lower.contains("not present locally")
                || lower.contains("fast-forward")
                || lower.contains("fast forward")
                || lower.contains("non-fast")
            {
                GitOpError::NonFastForward(format!(
                    "remote rejected push of {branch_name}: {}; pull/rebase first",
                    e.message()
                ))
            } else {
                GitOpError::from_git2(&e)
            }
        })?;

    if let Some(msg) = rejection.borrow().clone() {
        return Err(GitOpError::NonFastForward(format!(
            "remote rejected {msg}; pull/rebase first"
        )));
    }

    let pushed_oid = repo
        .refname_to_id(&format!("refs/heads/{branch_name}"))
        .map_err(|e| GitOpError::from_git2(&e))?
        .to_string();

    let mut config = repo.config().map_err(|e| GitOpError::from_git2(&e))?;
    let remote_key = format!("branch.{branch_name}.remote");
    let set_upstream = if config.get_string(&remote_key).is_err() {
        config
            .set_str(&remote_key, remote_name)
            .map_err(|e| GitOpError::from_git2(&e))?;
        config
            .set_str(
                &format!("branch.{branch_name}.merge"),
                &format!("refs/heads/{branch_name}"),
            )
            .map_err(|e| GitOpError::from_git2(&e))?;
        true
    } else {
        false
    };

    Ok(GitPushResult {
        remote: remote_name.to_owned(),
        branch: branch_name,
        pushed_oid,
        set_upstream,
    })
}

#[cfg(test)]
#[path = "ops_test.rs"]
mod ops_test;
