//! `git worktree`-backed [`WorktreeManager`] for desktop hosts.
//!
//! Shells out to the `git` CLI rooted at the configured repository root.
//! Worktrees live at `<repo_root>/.lingxi/worktrees/<flatten_slug(slug)>`
//! and use the branch-name prefix `worktree-` (NOT `lingxi/` or `claude/`).
//!
//! The branch prefix and path layout match claude-code's
//! `src/utils/worktree.ts` exactly — see plan M2-01 §"Critical 1:1 fidelity
//! items" for the rationale.
//!
//! Slug validation rules:
//! - Each `/`-separated segment is `[a-zA-Z0-9._-]+`.
//! - Total length 1..=64 chars.
//! - No empty segments.
//!
//! `/` is flattened to `+` for the on-disk directory name so the layout
//! stays flat. `+` is outside the allowlist so the mapping is injective.

use async_trait::async_trait;
use platform_api::{
    PatchFile, PatchFileStatus, SnapshotLimits, WorkspaceBase, WorktreeChangeSummary,
    WorktreeError, WorktreeHandle, WorktreeInfo, WorktreeManager, WorktreePatch,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::process::Command;

/// Maximum allowed total length of a worktree slug.
///
/// Matches `MAX_WORKTREE_SLUG_LENGTH` in claude-code's TS reference at
/// `src/utils/worktree.ts`.
pub const MAX_WORKTREE_SLUG_LENGTH: usize = 64;

/// Validate a caller-supplied worktree slug.
///
/// Rules (mirrors claude-code's `src/utils/worktree.ts`):
/// - Total length 1..=64 chars.
/// - Each `/`-separated segment matches `^[a-zA-Z0-9._-]+$`.
/// - No empty segments (rejects `"/foo"`, `"foo/"`, `"a//b"`, `""`).
///
/// Returns [`WorktreeError::InvalidSlug`] with a human-readable detail when
/// any rule fails. The detail is suitable for direct surfacing in `/doctor`
/// or CLI error output.
pub fn validate_worktree_slug(slug: &str) -> Result<(), WorktreeError> {
    // 1:1 with the binary `_Tt`: a length cap (64 = `oac`), then per-`/`-segment
    // checks — reject the `.`/`..` path segments, reject the reserved `.git`
    // directory name (case-insensitive, trailing dots stripped), and require the
    // allowed set `ytf=/^[a-zA-Z0-9._-]+$/` (which also rejects empty segments).
    // Error messages are byte-exact: the binary wraps the slug/segment in LITERAL
    // double-quotes (`"${e}"`/`"${t}"`), so we format `"{slug}"` — NOT `{slug:?}`
    // (Rust Debug quoting would escape differently).
    if slug.len() > MAX_WORKTREE_SLUG_LENGTH {
        return Err(WorktreeError::InvalidSlug(format!(
            "Invalid worktree name: must be {MAX_WORKTREE_SLUG_LENGTH} characters or fewer (got {})",
            slug.len()
        )));
    }
    for segment in slug.split('/') {
        if segment == "." || segment == ".." {
            return Err(WorktreeError::InvalidSlug(format!(
                "Invalid worktree name \"{slug}\": must not contain \".\" or \"..\" path segments"
            )));
        }
        if segment.to_lowercase().trim_end_matches('.') == ".git" {
            return Err(WorktreeError::InvalidSlug(format!(
                "Invalid worktree name \"{slug}\": \"{segment}\" is a reserved git directory name"
            )));
        }
        if segment.is_empty()
            || !segment
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-')
        {
            return Err(WorktreeError::InvalidSlug(format!(
                "Invalid worktree name \"{slug}\": each \"/\"-separated segment must be non-empty and contain only letters, digits, dots, underscores, and dashes"
            )));
        }
    }
    Ok(())
}

/// Parse `git worktree prune -v` stdout into pruned on-disk paths.
///
/// Each relevant line has the form `Removing worktrees/<name>: <reason>`.
/// We strip the `<name>` and resolve it against
/// `<repo_root>/.lingxi/worktrees/<name>` (where this codebase places its
/// worktrees per claude-code's layout). Lines that don't match the
/// expected prefix are silently skipped.
fn parse_prune_v_stdout(stdout: &str, repo_root: &std::path::Path) -> Vec<PathBuf> {
    stdout
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("Removing worktrees/")?;
            // <name>: <reason>
            let name = rest.split(':').next()?;
            if name.is_empty() {
                return None;
            }
            Some(
                repo_root
                    .join(branding::DOT_DIR)
                    .join("worktrees")
                    .join(name),
            )
        })
        .collect()
}

/// Count the non-blank lines of `git status --porcelain` stdout.
///
/// Byte-faithful to claude-code's
/// `count(status.stdout.split('\n'), l => l.trim() !== '')`
/// (`src/tools/ExitWorktreeTool/ExitWorktreeTool.ts:92`): split on `'\n'`
/// (NOT `lines()`, which also splits on `\r\n` and drops a trailing newline
/// differently), then count entries whose trimmed value is non-empty. A
/// trailing newline yields a final empty entry that is correctly excluded.
fn count_porcelain_changed_files(stdout: &str) -> usize {
    stdout.split('\n').filter(|l| !l.trim().is_empty()).count()
}

/// Flatten a `/`-separated slug into a single filesystem-friendly name.
///
/// Replaces every `/` with `+`. Because `+` is outside the allowed slug
/// character set (see [`validate_worktree_slug`]), this mapping is
/// injective: no two distinct valid slugs flatten to the same string.
///
/// Examples:
/// - `flatten_slug("user/feature")` → `"user+feature"`
/// - `flatten_slug("a/b/c")` → `"a+b+c"`
/// - `flatten_slug("plain")` → `"plain"`
#[must_use]
pub fn flatten_slug(slug: &str) -> String {
    slug.replace('/', "+")
}

/// Identity for the snapshot commits the host writes on its own behalf, so
/// `commit-tree` works in a repository with no `user.name` configured.
const SNAPSHOT_IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "Workspace snapshot"),
    ("GIT_AUTHOR_EMAIL", "snapshot@localhost"),
    ("GIT_COMMITTER_NAME", "Workspace snapshot"),
    ("GIT_COMMITTER_EMAIL", "snapshot@localhost"),
];

/// Pathspec keeping the managed worktrees (each a nested checkout) out of a
/// snapshot or patch; `git add -A` would otherwise record them as gitlinks.
fn managed_worktrees_exclude() -> String {
    format!(":(top,exclude){}/worktrees", branding::DOT_DIR)
}

/// Run `git` in `dir`, optionally against a private index file.
async fn git_in(
    dir: &Path,
    args: &[&str],
    index: Option<&Path>,
) -> Result<std::process::Output, WorktreeError> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    cmd.output()
        .await
        .map_err(|e| WorktreeError::Io(e.to_string()))
}

/// [`git_in`]'s stdout, or its stderr as a [`WorktreeError::Git`].
async fn git_stdout(
    dir: &Path,
    args: &[&str],
    index: Option<&Path>,
) -> Result<Vec<u8>, WorktreeError> {
    let output = git_in(dir, args, index).await?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(WorktreeError::Git(
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ))
    }
}

fn trimmed(stdout: &[u8]) -> String {
    String::from_utf8_lossy(stdout).trim().to_string()
}

/// A private copy of a checkout's index, so `git add -A` can stage the whole
/// working tree without touching the index the user (or panel) sees. Seeded
/// from the real index only to reuse its stat cache; an empty start gives the
/// same tree. Removed on drop.
struct TempIndex {
    path: PathBuf,
}

impl TempIndex {
    async fn seeded_from(checkout: &Path) -> Result<Self, WorktreeError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let source =
            trimmed(&git_stdout(checkout, &["rev-parse", "--git-path", "index"], None).await?);
        let path = std::env::temp_dir().join(format!(
            "worktree-index-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = tokio::fs::copy(checkout.join(source), &path).await;
        Ok(Self { path })
    }

    /// Stage everything in `checkout` except the managed worktrees.
    async fn add_all(&self, checkout: &Path) -> Result<(), WorktreeError> {
        let exclude = managed_worktrees_exclude();
        git_stdout(
            checkout,
            &["add", "-A", "--", ".", exclude.as_str()],
            Some(&self.path),
        )
        .await
        .map(drop)
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let mut lock = self.path.clone().into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(lock);
    }
}

/// Parse `git diff --numstat -z` into `(path, insertions, deletions, binary)`.
/// A rename's record is `ins\tdel\t\0from\0to\0`; the new path is kept.
fn parse_numstat_z(stdout: &str) -> Vec<(String, u32, u32, bool)> {
    let mut tokens = stdout.split('\0');
    let mut out = Vec::new();
    while let Some(record) = tokens.next() {
        if record.is_empty() {
            continue;
        }
        let mut parts = record.splitn(3, '\t');
        let (Some(ins), Some(del), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let path = if path.is_empty() {
            let _from = tokens.next();
            match tokens.next() {
                Some(to) => to.to_string(),
                None => break,
            }
        } else {
            path.to_string()
        };
        let binary = ins == "-" || del == "-";
        out.push((
            path,
            ins.parse().unwrap_or(0),
            del.parse().unwrap_or(0),
            binary,
        ));
    }
    out
}

/// Parse `git diff --name-status -z` into `(path, status)`. Copies are
/// reported as additions: the copy source is unchanged.
fn parse_name_status_z(stdout: &str) -> Vec<(String, PatchFileStatus)> {
    let mut tokens = stdout.split('\0');
    let mut out = Vec::new();
    while let Some(code) = tokens.next() {
        let Some(kind) = code.chars().next() else {
            continue;
        };
        let entry = match kind {
            'R' => match (tokens.next(), tokens.next()) {
                (Some(from), Some(to)) => (
                    to.to_string(),
                    PatchFileStatus::Renamed {
                        from: from.to_string(),
                    },
                ),
                _ => break,
            },
            'C' => match (tokens.next(), tokens.next()) {
                (Some(_from), Some(to)) => (to.to_string(), PatchFileStatus::Added),
                _ => break,
            },
            _ => {
                let Some(path) = tokens.next() else { break };
                let status = match kind {
                    'A' => PatchFileStatus::Added,
                    'D' => PatchFileStatus::Deleted,
                    _ => PatchFileStatus::Modified,
                };
                (path.to_string(), status)
            }
        };
        out.push(entry);
    }
    out
}

/// Production [`WorktreeManager`] using the `git worktree` CLI.
pub struct PosixWorktreeManager {
    /// Absolute path to the main repository working copy.
    repo_root: PathBuf,
}

impl PosixWorktreeManager {
    /// Build a new `PosixWorktreeManager` rooted at `repo_root`.
    ///
    /// New worktrees are created at
    /// `<repo_root>/.lingxi/worktrees/<flatten_slug(slug)>`. The layout is
    /// fixed (matches claude-code) — there is no `worktree_base` knob.
    #[must_use]
    pub fn new(repo_root: PathBuf) -> Self {
        Self { repo_root }
    }
}

#[async_trait]
impl WorktreeManager for PosixWorktreeManager {
    async fn create_worktree(
        &self,
        slug: &str,
        base_branch: Option<&str>,
        copy_includes: &[PathBuf],
    ) -> Result<WorktreeHandle, WorktreeError> {
        validate_worktree_slug(slug)?;
        let flat = flatten_slug(slug);
        let branch_name = format!("worktree-{flat}");
        let worktree_path = self
            .repo_root
            .join(branding::DOT_DIR)
            .join("worktrees")
            .join(&flat);

        // Refuse a repository-committed symlink at the managed dot-dir chain
        // before we `mkdir` through it or spawn `git worktree add` — a symlink
        // at `.lingxi`, `.lingxi/worktrees`, or the target could redirect the
        // checkout outside the repo. Byte-faithful port of CC 2.1.212 `yWi`,
        // called immediately before the worktree-add spawn.
        platform_common::reject_worktree_create_symlinks(
            &self.repo_root,
            branding::DOT_DIR,
            &worktree_path,
        )
        .await?;

        // git worktree add creates the leaf; the `.lingxi/worktrees/` parent
        // may not exist yet.
        if let Some(parent) = worktree_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| WorktreeError::Io(e.to_string()))?;
        }

        let mut cmd = Command::new("git");
        cmd.current_dir(&self.repo_root);
        cmd.arg("worktree")
            .arg("add")
            .arg("-b")
            .arg(&branch_name)
            .arg(&worktree_path);
        if let Some(base) = base_branch {
            cmd.arg(base);
        }

        let output = cmd
            .output()
            .await
            .map_err(|e| WorktreeError::Io(e.to_string()))?;
        if !output.status.success() {
            return Err(WorktreeError::Git(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }

        // Best-effort copy of caller-supplied include paths. Missing sources
        // are silently skipped — the intent is to ferry across e.g. `.env`
        // files that aren't tracked by git.
        for rel in copy_includes {
            let src = self.repo_root.join(rel);
            if !tokio::fs::try_exists(&src)
                .await
                .map_err(|e| WorktreeError::Io(e.to_string()))?
            {
                continue;
            }
            let dst = worktree_path.join(rel);
            if let Some(parent) = dst.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| WorktreeError::Io(e.to_string()))?;
            }
            tokio::fs::copy(&src, &dst)
                .await
                .map_err(|e| WorktreeError::Io(e.to_string()))?;
        }

        // Post-create `.worktreeinclude` copy — claude-code 2.1.207's
        // `copyWorktreeIncludeFiles` (fn `TZc`), the last step of the shared
        // post-create setup `H6i` that runs for BOTH the agent-isolation
        // worktree and the `--worktree` session flow. Copies the git-ignored
        // files the repo's `.worktreeinclude` selects (e.g. `.env`, `secrets/`)
        // into the fresh worktree. Best-effort/infallible, so it never fails a
        // successful `git worktree add`; runs alongside the literal
        // `copy_includes` above (which serves the EnterWorktree tool's input).
        platform_common::copy_worktree_include_files(&self.repo_root, &worktree_path).await;

        // Capture the worktree's initial HEAD — claude-code's
        // `originalHeadCommit` (the commit `git worktree add` checked out).
        // `worktree_change_summary` counts ahead-commits as
        // `rev-list --count <base>..HEAD`, so without this baseline a
        // clean-but-committed worktree would report `commits: 0` and be
        // auto-removed. Best-effort: a failure leaves `base_commit: None`,
        // which yields `commits: 0` (claude's `if (!headCommit)`).
        let base_commit = Command::new("git")
            .arg("-C")
            .arg(&worktree_path)
            .arg("rev-parse")
            .arg("HEAD")
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty());

        Ok(WorktreeHandle {
            path: worktree_path,
            branch_name,
            base_commit,
        })
    }

    async fn remove_worktree(&self, handle: &WorktreeHandle) -> Result<(), WorktreeError> {
        let output = Command::new("git")
            .current_dir(&self.repo_root)
            .arg("worktree")
            .arg("remove")
            .arg(&handle.path)
            .output()
            .await
            .map_err(|e| WorktreeError::Io(e.to_string()))?;
        if !output.status.success() {
            return Err(WorktreeError::Git(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        Ok(())
    }

    async fn list_worktrees(&self) -> Result<Vec<WorktreeInfo>, WorktreeError> {
        let output = Command::new("git")
            .current_dir(&self.repo_root)
            .arg("worktree")
            .arg("list")
            .arg("--porcelain")
            .output()
            .await
            .map_err(|e| WorktreeError::Io(e.to_string()))?;
        if !output.status.success() {
            return Err(WorktreeError::Git(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut out = Vec::new();
        let mut current: Option<WorktreeInfo> = None;
        for line in stdout.lines() {
            if let Some(rest) = line.strip_prefix("worktree ") {
                if let Some(c) = current.take() {
                    out.push(c);
                }
                current = Some(WorktreeInfo {
                    path: PathBuf::from(rest),
                    branch: String::new(),
                    created_at: std::time::SystemTime::now(),
                });
            } else if let Some(rest) = line.strip_prefix("branch ") {
                if let Some(c) = current.as_mut() {
                    // Binary: `a.slice(7).replace(/^refs\/heads\//,"")` — strip the
                    // `branch ` prefix (7 chars), then a leading `refs/heads/` if
                    // present (a porcelain `branch ` line need not carry it).
                    c.branch = rest.strip_prefix("refs/heads/").unwrap_or(rest).to_string();
                }
            }
        }
        if let Some(c) = current {
            out.push(c);
        }
        Ok(out)
    }

    async fn cleanup_stale(&self, _max_age: Duration) -> Result<Vec<PathBuf>, WorktreeError> {
        // `max_age` is currently unused: `git worktree prune` consults its
        // own `gc.worktreePruneExpire` setting. claude-code does not expose
        // a per-call override either. Explicit age filtering is M2-followup.
        let output = Command::new("git")
            .current_dir(&self.repo_root)
            .arg("worktree")
            .arg("prune")
            .arg("-v")
            .output()
            .await
            .map_err(|e| WorktreeError::Io(e.to_string()))?;
        if !output.status.success() {
            return Err(WorktreeError::Git(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(parse_prune_v_stdout(&stdout, &self.repo_root))
    }

    fn is_supported(&self) -> bool {
        true
    }

    async fn enter_existing(
        &self,
        path: &std::path::Path,
    ) -> Result<WorktreeHandle, WorktreeError> {
        // Verify `path` is itself the ROOT of a git worktree — not merely a
        // directory nested somewhere inside one. `--is-inside-work-tree`
        // would be too permissive here: it exits 0 for ANY subdirectory of a
        // checkout (e.g. `repo_root/src`, which is not a worktree root) and
        // also exits 0 (printing "false") for a bare repo. Task 7 feeds this
        // method MODEL-SUPPLIED paths, so a wrong path must not silently
        // succeed.
        //
        // `git -C <path> rev-parse --show-toplevel` prints the root of the
        // working tree containing `<path>`. Canonicalize both `path` and the
        // printed top-level and require them to be equal: a real worktree
        // root's top-level IS itself (equal → accept); a subdirectory's
        // top-level is its parent repo root (mismatch → reject); and the
        // command fails outright for a bare repo or a non-git directory
        // (reject).
        if !tokio::fs::try_exists(path)
            .await
            .map_err(|e| WorktreeError::Io(e.to_string()))?
        {
            return Err(WorktreeError::Io(format!(
                "worktree path does not exist: {}",
                path.display()
            )));
        }
        let canonical_path = tokio::fs::canonicalize(path)
            .await
            .map_err(|e| WorktreeError::Io(e.to_string()))?;

        let probe = Command::new("git")
            .arg("-C")
            .arg(path)
            .arg("rev-parse")
            .arg("--show-toplevel")
            .output()
            .await
            .map_err(|e| WorktreeError::Io(e.to_string()))?;
        if !probe.status.success() {
            return Err(WorktreeError::Git(format!(
                "not a git worktree: {}",
                path.display()
            )));
        }
        let toplevel = String::from_utf8_lossy(&probe.stdout).trim().to_string();
        let canonical_toplevel = tokio::fs::canonicalize(&toplevel)
            .await
            .map_err(|e| WorktreeError::Git(format!(
                "`git rev-parse --show-toplevel` for {} printed an unresolvable path {toplevel:?}: {e}",
                path.display()
            )))?;
        if canonical_toplevel != canonical_path {
            return Err(WorktreeError::Git(format!(
                "not a worktree root: {} is nested inside worktree/repo root {}",
                path.display(),
                canonical_toplevel.display()
            )));
        }

        // Resolve the checked-out branch. `--abbrev-ref HEAD` returns the
        // branch name, or the literal `HEAD` when detached — pass either
        // through as-is (no branch to report is still faithfully reported).
        let branch_out = Command::new("git")
            .arg("-C")
            .arg(path)
            .arg("rev-parse")
            .arg("--abbrev-ref")
            .arg("HEAD")
            .output()
            .await
            .map_err(|e| WorktreeError::Io(e.to_string()))?;
        if !branch_out.status.success() {
            return Err(WorktreeError::Git(
                String::from_utf8_lossy(&branch_out.stderr).into_owned(),
            ));
        }
        let branch_name = String::from_utf8_lossy(&branch_out.stdout)
            .trim()
            .to_string();

        Ok(WorktreeHandle {
            path: path.to_path_buf(),
            branch_name,
            // Unknown when entering an existing worktree — no creation-time
            // baseline was captured (matches `create_worktree`'s `None` on a
            // best-effort `rev-parse HEAD` failure).
            base_commit: None,
        })
    }

    async fn worktree_change_summary(
        &self,
        handle: &WorktreeHandle,
    ) -> Result<Option<WorktreeChangeSummary>, WorktreeError> {
        // Mirror claude-code `countWorktreeChanges`
        // (ExitWorktreeTool.ts:79-113): `git status --porcelain` for the
        // working-tree dirty count. Fail-closed (`Ok(None)`) on a non-zero
        // exit — a lock file, corrupt index, or non-git path. A spawn failure
        // (no git binary) is a hard `Git` error, matching the rest of this
        // impl's error surface.
        let status = Command::new("git")
            .arg("-C")
            .arg(&handle.path)
            .arg("status")
            .arg("--porcelain")
            .output()
            .await
            .map_err(|e| WorktreeError::Git(e.to_string()))?;
        if !status.status.success() {
            return Ok(None);
        }
        let stdout = String::from_utf8_lossy(&status.stdout);
        let changed_files = count_porcelain_changed_files(&stdout);

        // Ahead-commit count, 1:1 with the binary `LTl`: `git rev-list --count
        // <base>..HEAD` where `<base>` is the worktree's baseline captured at
        // creation ([`WorktreeHandle::base_commit`]). The binary returns `null`
        // for the WHOLE summary — NOT a {n,0} half-result — when there is no
        // baseline (`if (!t) return null`) or the rev-list spawn fails / exits
        // non-zero (`if (o.code !== 0) return null`); the caller then defaults to
        // `{0,0}` (`?? {changedFiles:0,commits:0}`) and emits no discard note.
        // Only a zero-exit-but-unparseable count degrades to `0` (`parseInt||0`).
        let Some(base) = &handle.base_commit else {
            return Ok(None);
        };
        let rev = Command::new("git")
            .arg("-C")
            .arg(&handle.path)
            .arg("rev-list")
            .arg("--count")
            .arg(format!("{base}..HEAD"))
            .output()
            .await;
        let rev = match rev {
            Ok(o) if o.status.success() => o,
            // Spawn failure or non-zero exit ⇒ null (whole summary discarded).
            _ => return Ok(None),
        };
        let commits = String::from_utf8_lossy(&rev.stdout)
            .trim()
            .parse::<usize>()
            .unwrap_or(0);

        Ok(Some(WorktreeChangeSummary {
            changed_files,
            commits,
        }))
    }

    async fn snapshot_base(&self, limits: SnapshotLimits) -> Result<WorkspaceBase, WorktreeError> {
        let top = PathBuf::from(trimmed(
            &git_stdout(&self.repo_root, &["rev-parse", "--show-toplevel"], None).await?,
        ));
        // Fails (quietly) only on an unborn branch.
        let head = git_in(&top, &["rev-parse", "--verify", "--quiet", "HEAD"], None).await?;
        let head = head
            .status
            .success()
            .then(|| trimmed(&head.stdout))
            .filter(|head| !head.is_empty());
        let exclude = managed_worktrees_exclude();
        let status = git_stdout(
            &top,
            &[
                "status",
                "--porcelain",
                "-z",
                "--untracked-files=all",
                "--",
                ".",
                exclude.as_str(),
            ],
            None,
        )
        .await?;
        if let (true, Some(head)) = (status.is_empty(), &head) {
            return Ok(WorkspaceBase {
                commit: head.clone(),
                head: Some(head.clone()),
                includes_uncommitted: false,
            });
        }

        let untracked = git_stdout(
            &top,
            &[
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
                ".",
                exclude.as_str(),
            ],
            None,
        )
        .await?;
        let untracked = String::from_utf8_lossy(&untracked);
        let mut files = 0usize;
        let mut bytes = 0u64;
        for path in untracked.split('\0').filter(|path| !path.is_empty()) {
            files += 1;
            if let Ok(meta) = tokio::fs::symlink_metadata(top.join(path)).await {
                bytes = bytes.saturating_add(meta.len());
            }
        }
        if files > limits.max_untracked_files || bytes > limits.max_untracked_bytes {
            return Err(WorktreeError::SnapshotRefused(format!(
                "The workspace has {files} untracked files ({bytes} bytes); the limit is {} files \
                 and {} bytes. Commit them or add them to .gitignore first.",
                limits.max_untracked_files, limits.max_untracked_bytes
            )));
        }

        let index = TempIndex::seeded_from(&top).await?;
        index.add_all(&top).await?;
        let tree = trimmed(&git_stdout(&top, &["write-tree"], Some(&index.path)).await?);
        let mut args = vec!["commit-tree", "--no-gpg-sign", "-m", "Workspace snapshot"];
        if let Some(head) = &head {
            args.extend(["-p", head.as_str()]);
        }
        args.push(tree.as_str());
        let mut cmd = Command::new("git");
        cmd.current_dir(&top).args(&args).envs(SNAPSHOT_IDENTITY);
        let output = cmd
            .output()
            .await
            .map_err(|e| WorktreeError::Io(e.to_string()))?;
        if !output.status.success() {
            return Err(WorktreeError::Git(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        Ok(WorkspaceBase {
            commit: trimmed(&output.stdout),
            head,
            includes_uncommitted: true,
        })
    }

    async fn worktree_patch(
        &self,
        handle: &WorktreeHandle,
        base: &str,
    ) -> Result<WorktreePatch, WorktreeError> {
        let dir = handle.path.as_path();
        let index = TempIndex::seeded_from(dir).await?;
        index.add_all(dir).await?;
        let diff_args = |extra: &[&'static str]| {
            let mut args: Vec<&str> = vec!["diff", "--cached", "-M", "--no-color", "--no-ext-diff"];
            args.extend_from_slice(extra);
            args.push(base);
            args
        };
        let diff = git_stdout(
            dir,
            &diff_args(&[
                "--binary",
                "--no-textconv",
                "--src-prefix=a/",
                "--dst-prefix=b/",
            ]),
            Some(&index.path),
        )
        .await?;
        let numstat = git_stdout(dir, &diff_args(&["--numstat", "-z"]), Some(&index.path)).await?;
        let statuses =
            git_stdout(dir, &diff_args(&["--name-status", "-z"]), Some(&index.path)).await?;
        let counts = parse_numstat_z(&String::from_utf8_lossy(&numstat));
        let files = parse_name_status_z(&String::from_utf8_lossy(&statuses))
            .into_iter()
            .map(|(path, status)| {
                let (insertions, deletions, binary) = counts
                    .iter()
                    .find(|(counted, ..)| *counted == path)
                    .map_or((0, 0, false), |(_, ins, del, binary)| (*ins, *del, *binary));
                PatchFile {
                    path,
                    status,
                    insertions,
                    deletions,
                    binary,
                }
            })
            .collect();
        Ok(WorktreePatch {
            diff: String::from_utf8_lossy(&diff).into_owned(),
            files,
        })
    }

    async fn discard_worktree(&self, handle: &WorktreeHandle) -> Result<(), WorktreeError> {
        let path = handle.path.to_string_lossy();
        let removed = git_in(
            &self.repo_root,
            &["worktree", "remove", "--force", path.as_ref()],
            None,
        )
        .await?;
        if !removed.status.success() {
            if tokio::fs::try_exists(&handle.path).await.unwrap_or(true) {
                return Err(WorktreeError::Git(
                    String::from_utf8_lossy(&removed.stderr).into_owned(),
                ));
            }
            // Already gone from disk: drop git's stale record of it.
            let _ = git_in(&self.repo_root, &["worktree", "prune"], None).await;
        }
        // Best effort: the branch may already be gone.
        let _ = git_in(
            &self.repo_root,
            &["branch", "-D", handle.branch_name.as_str()],
            None,
        )
        .await;
        Ok(())
    }
}

#[cfg(test)]
mod slug_tests {
    use super::*;

    #[test]
    fn validate_accepts_legal_slugs() {
        for ok in [
            "feature",
            "user",
            "v1.2.3",
            "with-dash",
            "with_under",
            "with.dot",
            "user/feature",
            "topic/area/sub",
        ] {
            assert!(validate_worktree_slug(ok).is_ok(), "expected ok: {ok:?}");
        }
    }

    #[test]
    fn validate_rejects_bad_chars_and_segments() {
        for bad in ["a*b", "a b", "a:b", "a+b", "", "/foo", "foo/", "a//b"] {
            assert!(
                matches!(
                    validate_worktree_slug(bad),
                    Err(WorktreeError::InvalidSlug(_))
                ),
                "expected err: {bad:?}"
            );
        }
    }

    #[test]
    fn validate_rejects_over_64_chars() {
        assert!(matches!(
            validate_worktree_slug(&"a".repeat(65)),
            Err(WorktreeError::InvalidSlug(_)),
        ));
        assert!(validate_worktree_slug(&"a".repeat(64)).is_ok());
    }

    // Binary `_Tt` per-segment rules: reject `.`/`..` path segments and the
    // reserved `.git` directory name (case-insensitive, trailing dots stripped).
    #[test]
    fn validate_rejects_dot_dotdot_and_dotgit_segments() {
        for bad in [
            ".", "..", "foo/.", "foo/..", "../x", ".git", ".GIT", ".git.", ".git...", "a/.git",
            "a/.git/b",
        ] {
            assert!(
                matches!(
                    validate_worktree_slug(bad),
                    Err(WorktreeError::InvalidSlug(_))
                ),
                "expected err: {bad:?}"
            );
        }
        // A `.git`-prefixed name that is NOT exactly the reserved dir is fine.
        assert!(validate_worktree_slug(".gitfoo").is_ok());
        assert!(validate_worktree_slug("foo.git").is_ok());
    }

    // Byte-exact error wording (binary `_Tt`, literal double-quotes around the
    // slug/segment — not Rust Debug quoting).
    #[test]
    fn validate_error_messages_are_byte_exact() {
        let msg = |s: &str| match validate_worktree_slug(s) {
            Err(WorktreeError::InvalidSlug(d)) => d,
            other => panic!("expected InvalidSlug, got {other:?}"),
        };
        assert_eq!(
            msg(".."),
            "Invalid worktree name \"..\": must not contain \".\" or \"..\" path segments"
        );
        assert_eq!(
            msg(".git"),
            "Invalid worktree name \".git\": \".git\" is a reserved git directory name"
        );
        assert_eq!(
            msg("a b"),
            "Invalid worktree name \"a b\": each \"/\"-separated segment must be non-empty and contain only letters, digits, dots, underscores, and dashes"
        );
        assert_eq!(
            msg(&"a".repeat(65)),
            "Invalid worktree name: must be 64 characters or fewer (got 65)"
        );
    }

    #[test]
    fn flatten_replaces_slashes_with_plus() {
        assert_eq!(flatten_slug("user/feature"), "user+feature");
        assert_eq!(flatten_slug("a/b/c"), "a+b+c");
        assert_eq!(flatten_slug("plain"), "plain");
    }

    #[test]
    fn flatten_is_injective_for_valid_slugs() {
        // The key property: no two valid slugs flatten to the same string,
        // because `+` is outside the allowed character set. `a+b` is not a
        // valid slug so it cannot collide with `a/b`'s flattened form.
        assert_ne!(flatten_slug("a/b"), flatten_slug("ab"));
        assert_ne!(flatten_slug("a/b/c"), flatten_slug("ab/c"));
        assert!(validate_worktree_slug("a+b").is_err());
    }
}

#[cfg(test)]
mod create_tests {
    use super::*;
    use platform_api::WorktreeManager;
    use tempfile::TempDir;
    use tokio::process::Command;

    /// Initialize a fresh git repo with one commit so worktree commands have
    /// something to branch from.
    async fn init_repo(dir: &std::path::Path) {
        async fn git(dir: &std::path::Path, args: &[&str]) {
            let mut c = Command::new("git");
            c.current_dir(dir);
            for a in args {
                c.arg(a);
            }
            assert!(
                c.output().await.unwrap().status.success(),
                "git {args:?} failed"
            );
        }
        git(dir, &["init", "-q", "-b", "main"]).await;
        git(dir, &["config", "user.email", "ci@test"]).await;
        git(dir, &["config", "user.name", "ci"]).await;
        tokio::fs::write(dir.join("seed.txt"), "seed")
            .await
            .unwrap();
        git(dir, &["add", "seed.txt"]).await;
        git(dir, &["commit", "-qm", "seed"]).await;
    }

    #[tokio::test]
    async fn create_uses_worktree_dash_prefix_and_dot_claude_layout() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().to_path_buf();
        init_repo(&repo).await;
        let handle = PosixWorktreeManager::new(repo.clone())
            .create_worktree("user/feature", None, &[])
            .await
            .unwrap();
        assert_eq!(handle.branch_name, "worktree-user+feature");
        assert_eq!(handle.path, repo.join(".lingxi/worktrees/user+feature"));
        assert!(handle.path.exists());
    }

    #[tokio::test]
    async fn create_rejects_invalid_slug_before_running_git() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().to_path_buf();
        init_repo(&repo).await;
        let err = PosixWorktreeManager::new(repo)
            .create_worktree("a*b", None, &[])
            .await
            .unwrap_err();
        assert!(matches!(err, WorktreeError::InvalidSlug(_)));
    }

    #[tokio::test]
    async fn create_copies_includes_when_source_exists() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().to_path_buf();
        init_repo(&repo).await;
        tokio::fs::write(repo.join(".env"), "API_KEY=secret")
            .await
            .unwrap();
        let handle = PosixWorktreeManager::new(repo.clone())
            .create_worktree("user/feature", None, &[std::path::PathBuf::from(".env")])
            .await
            .unwrap();
        let copied = tokio::fs::read_to_string(handle.path.join(".env"))
            .await
            .unwrap();
        assert_eq!(copied, "API_KEY=secret");
    }

    #[tokio::test]
    async fn create_runs_worktreeinclude_copy() {
        // End-to-end: a repo whose `.gitignore` ignores `.env` and whose
        // `.worktreeinclude` names `.env` must ferry that untracked-ignored file
        // into the new worktree via the post-create `copyWorktreeIncludeFiles`
        // step wired into `create_worktree` (parity 2.1.207 P2-03).
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().to_path_buf();
        init_repo(&repo).await;
        tokio::fs::write(repo.join(".gitignore"), ".env\n")
            .await
            .unwrap();
        tokio::fs::write(repo.join(".env"), "API_KEY=secret")
            .await
            .unwrap();
        tokio::fs::write(repo.join(".worktreeinclude"), ".env\n")
            .await
            .unwrap();
        let handle = PosixWorktreeManager::new(repo.clone())
            .create_worktree("feat", None, &[])
            .await
            .unwrap();
        let copied = tokio::fs::read_to_string(handle.path.join(".env"))
            .await
            .expect(".worktreeinclude entry copied into worktree");
        assert_eq!(copied, "API_KEY=secret");
    }

    #[tokio::test]
    async fn enter_existing_resolves_branch_and_path_of_real_worktree() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().to_path_buf();
        init_repo(&repo).await;
        let handle = PosixWorktreeManager::new(repo.clone())
            .create_worktree("feature", None, &[])
            .await
            .unwrap();

        let entered = PosixWorktreeManager::new(repo)
            .enter_existing(&handle.path)
            .await
            .unwrap();
        assert_eq!(entered.path, handle.path);
        assert_eq!(entered.branch_name, "worktree-feature");
        assert_eq!(entered.base_commit, None);
    }

    #[tokio::test]
    async fn enter_existing_errors_on_missing_path() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().to_path_buf();
        init_repo(&repo).await;
        let missing = repo.join("does-not-exist");
        let err = PosixWorktreeManager::new(repo)
            .enter_existing(&missing)
            .await
            .unwrap_err();
        assert!(matches!(err, WorktreeError::Io(_)));
    }

    #[tokio::test]
    async fn enter_existing_errors_on_non_worktree_directory() {
        // A repo whose manager we probe with, and an entirely separate
        // tempdir (no `git init` at all, not nested inside any repo) that
        // is not a git worktree.
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().to_path_buf();
        init_repo(&repo).await;
        let not_git_dir = TempDir::new().unwrap();
        let not_git = not_git_dir.path().to_path_buf();
        let err = PosixWorktreeManager::new(repo)
            .enter_existing(&not_git)
            .await
            .unwrap_err();
        assert!(matches!(err, WorktreeError::Git(_)));
    }

    #[tokio::test]
    async fn enter_existing_rejects_subdirectory_that_is_not_a_worktree_root() {
        // `repo/src` is genuinely inside a git working tree, so the old
        // `--is-inside-work-tree` probe would wrongly accept it. It is NOT a
        // worktree root — `--show-toplevel` from inside it resolves to
        // `repo`, not `repo/src` — so `enter_existing` must reject it.
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().to_path_buf();
        init_repo(&repo).await;
        let subdir = repo.join("src");
        tokio::fs::create_dir(&subdir).await.unwrap();

        let err = PosixWorktreeManager::new(repo)
            .enter_existing(&subdir)
            .await
            .unwrap_err();
        assert!(matches!(err, WorktreeError::Git(_)));
    }

    #[tokio::test]
    async fn create_rejects_symlinked_worktrees_dir_before_running_git() {
        // A repository-committed symlink at `.lingxi/worktrees` (pointing
        // outside the repo) must be refused with the byte-faithful message
        // before `git worktree add` runs — CC 2.1.212 `yWi`.
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().to_path_buf();
        init_repo(&repo).await;
        let outside = TempDir::new().unwrap();
        tokio::fs::create_dir_all(repo.join(".lingxi"))
            .await
            .unwrap();
        tokio::fs::symlink(outside.path(), repo.join(".lingxi/worktrees"))
            .await
            .unwrap();

        let err = PosixWorktreeManager::new(repo.clone())
            .create_worktree("feat", None, &[])
            .await
            .unwrap_err();
        match err {
            WorktreeError::SymlinkRejected(msg) => {
                assert!(msg.starts_with("Cannot create worktree: "), "{msg}");
                assert!(
                    msg.contains(
                        "is a symlink. A repository-committed symlink at .lingxi, .lingxi/worktrees, or .lingxi/worktrees/<name> could redirect worktree creation outside the repository. Remove the symlink and retry."
                    ),
                    "byte-faithful message: {msg}"
                );
            }
            other => panic!("expected SymlinkRejected, got {other:?}"),
        }
        // The symlink target must be untouched — no worktree was checked out
        // through the redirect.
        let mut entries = tokio::fs::read_dir(outside.path()).await.unwrap();
        assert!(
            entries.next_entry().await.unwrap().is_none(),
            "no checkout should have been written through the symlink"
        );
    }

    #[tokio::test]
    async fn create_skips_missing_copy_includes() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().to_path_buf();
        init_repo(&repo).await;
        let res = PosixWorktreeManager::new(repo)
            .create_worktree(
                "feat",
                None,
                &[std::path::PathBuf::from("does-not-exist.txt")],
            )
            .await;
        assert!(res.is_ok());
    }
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn parse_prune_output_extracts_names() {
        let stdout = "\
Removing worktrees/user+feature: gitdir file points to non-existent location
Removing worktrees/topic+area: gitdir file points to non-existent location
";
        let paths = parse_prune_v_stdout(stdout, &PathBuf::from("/tmp/repo"));
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/tmp/repo/.lingxi/worktrees/user+feature"),
                PathBuf::from("/tmp/repo/.lingxi/worktrees/topic+area"),
            ]
        );
    }

    #[test]
    fn parse_prune_output_ignores_unrelated_lines() {
        let stdout = "some random noise\nRemoving worktrees/ok: stale\nnot a removing line\n";
        let paths = parse_prune_v_stdout(stdout, &PathBuf::from("/r"));
        assert_eq!(paths, vec![PathBuf::from("/r/.lingxi/worktrees/ok")]);
    }

    #[test]
    fn parse_prune_output_empty_stdout() {
        assert!(parse_prune_v_stdout("", &PathBuf::from("/r")).is_empty());
    }
}

#[cfg(test)]
mod change_summary_tests {
    use super::*;
    use platform_api::WorktreeManager;
    use tempfile::TempDir;
    use tokio::process::Command;

    #[test]
    fn count_porcelain_clean_is_zero() {
        assert_eq!(count_porcelain_changed_files(""), 0);
        // Even a lone trailing newline (git's empty-status output) is zero.
        assert_eq!(count_porcelain_changed_files("\n"), 0);
    }

    #[test]
    fn count_porcelain_counts_non_blank_lines() {
        // Two changed entries with a trailing newline → 2 (the trailing
        // empty split entry is excluded, matching the TS `trim() !== ''`).
        let stdout = " M src/a.rs\n?? new.txt\n";
        assert_eq!(count_porcelain_changed_files(stdout), 2);
    }

    #[test]
    fn count_porcelain_ignores_whitespace_only_lines() {
        let stdout = " M a\n   \n A b\n";
        assert_eq!(count_porcelain_changed_files(stdout), 2);
    }

    async fn git(dir: &std::path::Path, args: &[&str]) {
        let mut c = Command::new("git");
        c.current_dir(dir);
        for a in args {
            c.arg(a);
        }
        assert!(
            c.output().await.unwrap().status.success(),
            "git {args:?} failed"
        );
    }

    async fn head_sha(dir: &std::path::Path) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .await
            .unwrap();
        assert!(out.status.success(), "git rev-parse HEAD failed");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    /// Deterministic git repo: one commit, then a controllable dirty state.
    async fn init_repo(dir: &std::path::Path) {
        git(dir, &["init", "-q", "-b", "main"]).await;
        git(dir, &["config", "user.email", "ci@test"]).await;
        git(dir, &["config", "user.name", "ci"]).await;
        tokio::fs::write(dir.join("seed.txt"), "seed")
            .await
            .unwrap();
        git(dir, &["add", "seed.txt"]).await;
        git(dir, &["commit", "-qm", "seed"]).await;
    }

    #[tokio::test]
    async fn change_summary_clean_repo_is_zero() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().to_path_buf();
        init_repo(&repo).await;
        // A baseline is required for a Some summary (binary `LTl` `if(!t)return
        // null`); use HEAD so rev-list HEAD..HEAD = 0 commits.
        let base = head_sha(&repo).await;
        let handle = WorktreeHandle {
            path: repo.clone(),
            branch_name: "main".into(),
            base_commit: Some(base),
        };
        let summary = PosixWorktreeManager::new(repo)
            .worktree_change_summary(&handle)
            .await
            .unwrap()
            .expect("git status succeeds → Some");
        assert_eq!(summary.changed_files, 0);
        assert_eq!(summary.commits, 0);
        assert!(!summary.is_dirty());
    }

    #[tokio::test]
    async fn change_summary_counts_uncommitted_files() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().to_path_buf();
        init_repo(&repo).await;
        // One tracked-file modification + one untracked file = 2 porcelain
        // lines.
        tokio::fs::write(repo.join("seed.txt"), "changed")
            .await
            .unwrap();
        tokio::fs::write(repo.join("untracked.txt"), "x")
            .await
            .unwrap();
        let base = head_sha(&repo).await;
        let handle = WorktreeHandle {
            path: repo.clone(),
            branch_name: "main".into(),
            base_commit: Some(base),
        };
        let summary = PosixWorktreeManager::new(repo)
            .worktree_change_summary(&handle)
            .await
            .unwrap()
            .expect("git status succeeds → Some");
        assert_eq!(summary.changed_files, 2);
        assert!(summary.is_dirty());
        assert_eq!(
            summary.changed_files_phrase(),
            Some("2 uncommitted files".to_string())
        );
    }

    #[tokio::test]
    async fn change_summary_no_base_is_none() {
        // Binary `LTl`: `if (!t) return null` — no baseline ⇒ the WHOLE summary
        // is None (caller defaults to {0,0}, emits no discard note), even with a
        // dirty working tree.
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().to_path_buf();
        init_repo(&repo).await;
        tokio::fs::write(repo.join("seed.txt"), "changed")
            .await
            .unwrap();
        let handle = WorktreeHandle {
            path: repo.clone(),
            branch_name: "main".into(),
            base_commit: None,
        };
        let summary = PosixWorktreeManager::new(repo)
            .worktree_change_summary(&handle)
            .await
            .unwrap();
        assert!(
            summary.is_none(),
            "no baseline ⇒ None, not a {{n,0}} half-result"
        );
    }

    #[tokio::test]
    async fn change_summary_non_git_path_fails_closed() {
        // A directory that is not a git repo → git status exits non-zero →
        // Ok(None) (fail-closed "unknown").
        let tmp = TempDir::new().unwrap();
        let not_git = tmp.path().to_path_buf();
        let handle = WorktreeHandle {
            path: not_git.clone(),
            branch_name: "x".into(),
            base_commit: None,
        };
        let summary = PosixWorktreeManager::new(not_git)
            .worktree_change_summary(&handle)
            .await
            .unwrap();
        assert_eq!(summary, None, "non-git path must fail-closed to None");
    }

    #[tokio::test]
    async fn change_summary_counts_ahead_commits_with_clean_tree() {
        // The data-loss case: a CLEAN working tree (no porcelain lines) that
        // carries commits ahead of its base must still be reported dirty so the
        // worktree is KEPT, not auto-removed. Mirrors claude-code's keep half:
        // `commitsAhead = rev-list --count <originalHeadCommit>..HEAD`.
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().to_path_buf();
        init_repo(&repo).await;
        // Baseline = HEAD right after the seed commit.
        let base = String::from_utf8(
            Command::new("git")
                .current_dir(&repo)
                .args(["rev-parse", "HEAD"])
                .output()
                .await
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        // A second commit, leaving the working tree CLEAN.
        tokio::fs::write(repo.join("seed.txt"), "v2").await.unwrap();
        git(&repo, &["commit", "-qam", "v2"]).await;

        let handle = WorktreeHandle {
            path: repo.clone(),
            branch_name: "main".into(),
            base_commit: Some(base),
        };
        let summary = PosixWorktreeManager::new(repo)
            .worktree_change_summary(&handle)
            .await
            .unwrap()
            .expect("git status succeeds → Some");
        assert_eq!(summary.changed_files, 0, "working tree is clean");
        assert_eq!(summary.commits, 1, "one commit ahead of base");
        assert!(
            summary.is_dirty(),
            "clean-but-committed worktree must be kept (is_dirty via commits)"
        );
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use tempfile::TempDir;

    const LIMITS: SnapshotLimits = SnapshotLimits {
        max_untracked_files: 100,
        max_untracked_bytes: 1 << 20,
    };

    async fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .await
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    async fn repo() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]).await;
        git(&dir, &["config", "user.email", "ci@test"]).await;
        git(&dir, &["config", "user.name", "ci"]).await;
        std::fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
        std::fs::write(dir.join("keep.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.join("gone.txt"), "bye\n").unwrap();
        std::fs::write(dir.join("old.txt"), "a\nb\nc\nd\ne\nf\n").unwrap();
        git(&dir, &["add", "."]).await;
        git(&dir, &["commit", "-qm", "seed"]).await;
        (tmp, dir)
    }

    #[tokio::test]
    async fn a_clean_workspace_snapshots_to_head() {
        let (_tmp, dir) = repo().await;
        let base = PosixWorktreeManager::new(dir.clone())
            .snapshot_base(LIMITS)
            .await
            .unwrap();
        let head = git(&dir, &["rev-parse", "HEAD"]).await.trim().to_string();
        assert_eq!(base.commit, head);
        assert_eq!(base.head.as_deref(), Some(head.as_str()));
        assert!(!base.includes_uncommitted);
    }

    #[tokio::test]
    async fn a_dirty_workspace_snapshots_its_changes_without_touching_the_index() {
        let (_tmp, dir) = repo().await;
        std::fs::write(dir.join("keep.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::remove_file(dir.join("gone.txt")).unwrap();
        std::fs::write(dir.join("new.txt"), "fresh\n").unwrap();
        std::fs::write(dir.join("debug.log"), "ignored\n").unwrap();
        std::fs::write(dir.join("staged.txt"), "staged\n").unwrap();
        git(&dir, &["add", "staged.txt"]).await;
        let status_before = git(&dir, &["status", "--porcelain"]).await;

        let manager = PosixWorktreeManager::new(dir.clone());
        let base = manager.snapshot_base(LIMITS).await.unwrap();

        assert!(base.includes_uncommitted);
        let head = git(&dir, &["rev-parse", "HEAD"]).await.trim().to_string();
        assert_eq!(base.head.as_deref(), Some(head.as_str()));
        let parent = git(&dir, &["rev-parse", &format!("{}^", base.commit)]).await;
        assert_eq!(parent.trim(), head);
        let files = git(&dir, &["ls-tree", "-r", "--name-only", &base.commit]).await;
        let files: Vec<&str> = files.lines().collect();
        assert!(files.contains(&"new.txt") && files.contains(&"staged.txt"));
        assert!(!files.contains(&"gone.txt") && !files.contains(&"debug.log"));
        let keep = git(&dir, &["show", &format!("{}:keep.txt", base.commit)]).await;
        assert_eq!(keep, "one\ntwo\nthree\n");
        assert_eq!(git(&dir, &["status", "--porcelain"]).await, status_before);
        assert_eq!(git(&dir, &["stash", "list"]).await, "");

        // A worktree created from the snapshot sees the uncommitted work.
        let wt = manager
            .create_worktree("fusion-snap-p1", Some(&base.commit), &[])
            .await
            .unwrap();
        assert!(wt.path.join("new.txt").exists());
        assert!(!wt.path.join("gone.txt").exists());
    }

    #[tokio::test]
    async fn the_managed_worktrees_stay_out_of_a_snapshot() {
        let (_tmp, dir) = repo().await;
        let manager = PosixWorktreeManager::new(dir.clone());
        manager.create_worktree("other", None, &[]).await.unwrap();
        std::fs::write(dir.join("new.txt"), "fresh\n").unwrap();
        let base = manager.snapshot_base(LIMITS).await.unwrap();
        let files = git(&dir, &["ls-tree", "-r", "--name-only", &base.commit]).await;
        assert!(files
            .lines()
            .all(|file| !file.starts_with(branding::DOT_DIR)));
    }

    #[tokio::test]
    async fn too_many_untracked_files_refuse_the_snapshot() {
        let (_tmp, dir) = repo().await;
        for n in 0..3 {
            std::fs::write(dir.join(format!("u{n}.txt")), "x").unwrap();
        }
        let manager = PosixWorktreeManager::new(dir.clone());
        let limits = SnapshotLimits {
            max_untracked_files: 2,
            max_untracked_bytes: 1 << 20,
        };
        let err = manager.snapshot_base(limits).await.unwrap_err();
        assert!(
            matches!(err, WorktreeError::SnapshotRefused(ref msg) if msg.contains("3 untracked files"))
        );
        let limits = SnapshotLimits {
            max_untracked_files: 10,
            max_untracked_bytes: 2,
        };
        assert!(matches!(
            manager.snapshot_base(limits).await,
            Err(WorktreeError::SnapshotRefused(_))
        ));
    }

    #[tokio::test]
    async fn the_patch_covers_commits_and_uncommitted_work_and_applies_to_the_base() {
        let (_tmp, dir) = repo().await;
        let manager = PosixWorktreeManager::new(dir.clone());
        let base = manager.snapshot_base(LIMITS).await.unwrap();
        let wt = manager
            .create_worktree("fusion-patch-p1", Some(&base.commit), &[])
            .await
            .unwrap();
        std::fs::write(wt.path.join("keep.txt"), "one\n2\n").unwrap();
        git(&wt.path, &["commit", "-qam", "panel commit"]).await;
        std::fs::remove_file(wt.path.join("gone.txt")).unwrap();
        std::fs::write(wt.path.join("added.txt"), "new\n").unwrap();
        std::fs::write(wt.path.join("bin.dat"), [0u8, 159, 146, 150]).unwrap();
        git(&wt.path, &["mv", "old.txt", "moved.txt"]).await;
        std::fs::write(wt.path.join("trace.log"), "ignored\n").unwrap();
        let status_before = git(&wt.path, &["status", "--porcelain"]).await;

        let patch = manager.worktree_patch(&wt, &base.commit).await.unwrap();

        let find = |path: &str| patch.files.iter().find(|file| file.path == path).cloned();
        let keep = find("keep.txt").unwrap();
        assert_eq!(keep.status, PatchFileStatus::Modified);
        assert_eq!((keep.insertions, keep.deletions), (1, 1));
        assert_eq!(find("gone.txt").unwrap().status, PatchFileStatus::Deleted);
        assert_eq!(find("added.txt").unwrap().status, PatchFileStatus::Added);
        assert!(find("bin.dat").unwrap().binary);
        assert_eq!(
            find("moved.txt").unwrap().status,
            PatchFileStatus::Renamed {
                from: "old.txt".into()
            }
        );
        assert!(find("trace.log").is_none());
        assert_eq!(patch.files.len(), 5);
        assert_eq!(
            git(&wt.path, &["status", "--porcelain"]).await,
            status_before
        );

        let fresh = manager
            .create_worktree("fusion-patch-apply", Some(&base.commit), &[])
            .await
            .unwrap();
        let patch_file = dir.join("panel.patch");
        std::fs::write(&patch_file, &patch.diff).unwrap();
        git(&fresh.path, &["apply", patch_file.to_str().unwrap()]).await;
        assert_eq!(
            std::fs::read_to_string(fresh.path.join("keep.txt")).unwrap(),
            "one\n2\n"
        );
        assert_eq!(
            std::fs::read(fresh.path.join("bin.dat")).unwrap(),
            [0u8, 159, 146, 150]
        );
    }

    #[tokio::test]
    async fn an_untouched_worktree_has_an_empty_patch() {
        let (_tmp, dir) = repo().await;
        let manager = PosixWorktreeManager::new(dir.clone());
        let base = manager.snapshot_base(LIMITS).await.unwrap();
        let wt = manager
            .create_worktree("fusion-empty-p1", Some(&base.commit), &[])
            .await
            .unwrap();
        let patch = manager.worktree_patch(&wt, &base.commit).await.unwrap();
        assert!(patch.diff.is_empty());
        assert!(patch.files.is_empty());
    }

    #[tokio::test]
    async fn discarding_removes_a_dirty_worktree_and_its_branch_idempotently() {
        let (_tmp, dir) = repo().await;
        let manager = PosixWorktreeManager::new(dir.clone());
        let wt = manager
            .create_worktree("fusion-discard-p1", None, &[])
            .await
            .unwrap();
        std::fs::write(wt.path.join("dirty.txt"), "x").unwrap();
        manager.discard_worktree(&wt).await.unwrap();
        assert!(!wt.path.exists());
        let branches = git(&dir, &["branch", "--list", &wt.branch_name]).await;
        assert_eq!(branches, "");
        manager.discard_worktree(&wt).await.unwrap();
    }

    #[test]
    fn numstat_and_name_status_parse_renames_and_binaries() {
        let numstat = "1\t2\tsrc/a.rs\0-\t-\timg.png\x000\t0\t\0old.rs\0new.rs\0";
        assert_eq!(
            parse_numstat_z(numstat),
            vec![
                ("src/a.rs".to_string(), 1, 2, false),
                ("img.png".to_string(), 0, 0, true),
                ("new.rs".to_string(), 0, 0, false),
            ]
        );
        let statuses = "M\0src/a.rs\0R087\0old.rs\0new.rs\0A\0img.png\0D\0x\0C100\0a\0b\0";
        assert_eq!(
            parse_name_status_z(statuses),
            vec![
                ("src/a.rs".to_string(), PatchFileStatus::Modified),
                (
                    "new.rs".to_string(),
                    PatchFileStatus::Renamed {
                        from: "old.rs".into()
                    }
                ),
                ("img.png".to_string(), PatchFileStatus::Added),
                ("x".to_string(), PatchFileStatus::Deleted),
                ("b".to_string(), PatchFileStatus::Added),
            ]
        );
    }
}

/// Bytes free on the filesystem holding `path`, or `None` when it cannot be
/// read (a missing path, an unsupported filesystem).
#[must_use]
pub fn available_disk_bytes(path: &Path) -> Option<u64> {
    fs2::available_space(path).ok()
}
