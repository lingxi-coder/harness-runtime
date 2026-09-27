//! `git worktree`-backed [`WorktreeManager`] for Windows hosts.
//!
//! Shells out to the `git` CLI rooted at the configured repository root.
//! Worktrees live at `<repo_root>\.claude\worktrees\<flatten_slug(slug)>`
//! and use the branch-name prefix `worktree-`. Behavioral parity with the
//! posix implementation is intentional — see
//! [`platform_posix::worktree`] for the canonical doc.

use async_trait::async_trait;
use platform_api::{WorktreeError, WorktreeHandle, WorktreeInfo, WorktreeManager};
use std::path::PathBuf;
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
    // double-quotes (`"${e}"`/`"${t}"`), so we format `"{slug}"` — NOT `{slug:?}`.
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

/// Production [`WorktreeManager`] using the `git worktree` CLI.
pub struct WindowsWorktreeManager {
    /// Absolute path to the main repository working copy.
    repo_root: PathBuf,
}

impl WindowsWorktreeManager {
    /// Build a new `WindowsWorktreeManager` rooted at `repo_root`.
    ///
    /// Worktrees live at `<repo_root>\.claude\worktrees\<flatten_slug(slug)>`.
    #[must_use]
    pub fn new(repo_root: PathBuf) -> Self {
        Self { repo_root }
    }
}

#[async_trait]
impl WorktreeManager for WindowsWorktreeManager {
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
        // `originalHeadCommit` — so `worktree_change_summary` can count
        // ahead-commits (`rev-list --count <base>..HEAD`) and a
        // clean-but-committed worktree is kept rather than auto-removed.
        // Best-effort: a failure leaves `base_commit: None` ⇒ `commits: 0`.
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
                    // `branch ` prefix, then a leading `refs/heads/` if present.
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
        let handle = WindowsWorktreeManager::new(repo.clone())
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
        let err = WindowsWorktreeManager::new(repo)
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
        let handle = WindowsWorktreeManager::new(repo.clone())
            .create_worktree("user/feature", None, &[std::path::PathBuf::from(".env")])
            .await
            .unwrap();
        let copied = tokio::fs::read_to_string(handle.path.join(".env"))
            .await
            .unwrap();
        assert_eq!(copied, "API_KEY=secret");
    }

    #[tokio::test]
    async fn create_skips_missing_copy_includes() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().to_path_buf();
        init_repo(&repo).await;
        let res = WindowsWorktreeManager::new(repo)
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
